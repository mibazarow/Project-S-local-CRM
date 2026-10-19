// macOS system-config tools (Q.md computer-control slice): keychain_get
// reads one generic password from the Keychain via /usr/bin/security,
// defaults_write sets one preference key via /usr/bin/defaults. Both always
// confirm (Check): the first exposes a secret, the second rewrites user
// preferences. Values travel only as argv elements (no shell); leading "-"
// and control characters are refused so nothing can be read back as a flag,
// and typed defaults values are parsed before they reach the CLI. The
// secret is returned once and never appears in an error message.
use super::system_tools::run_cmd;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SECURITY_BINARY: &str = "/usr/bin/security";
const DEFAULTS_BINARY: &str = "/usr/bin/defaults";
const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const TYPES: [&str; 4] = ["string", "int", "bool", "float"];

// A value embedded as one argv element: non-empty, no leading dash (that
// would be parsed as an option), no control characters.
fn safe_field(name: &str, value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!("{name} must not start with '-'"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{name} must not contain control characters"));
    }
    Ok(())
}

fn missing_binary(path: &Path) -> ToolResult {
    ToolResult::err(
        ToolErrorCode::NotFound,
        format!("{} not found; it ships with every macOS install", path.display()),
    )
}

// ---------------------------------------------------------------------------
// keychain_get
// ---------------------------------------------------------------------------

fn keychain_argv(service: &str, account: &str) -> std::result::Result<Vec<String>, String> {
    safe_field("service", service)?;
    safe_field("account", account)?;
    Ok(vec![
        "find-generic-password".into(),
        "-w".into(),
        "-s".into(),
        service.to_string(),
        "-a".into(),
        account.to_string(),
    ])
}

pub struct KeychainGetTool {
    binary: PathBuf,
}

impl Default for KeychainGetTool {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(SECURITY_BINARY),
        }
    }
}

impl KeychainGetTool {
    #[cfg(test)]
    fn with_binary(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl Tool for KeychainGetTool {
    fn name(&self) -> &str {
        "keychain_get"
    }
    fn description(&self) -> &str {
        "Read one generic password from the macOS Keychain via /usr/bin/security.\nfind-generic-password -w -s <service> -a <account>; returns {ok, service, account, secret}. The secret is never written to logs or error messages; missing items are not_found. Always confirmed: the result exposes a credential to the conversation."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "service": { "type": "string" },
                "account": { "type": "string" }
            },
            "required": ["service", "account"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some(format!(
            "чтение секрета из Keychain: {} / {}",
            args.get("service").and_then(Value::as_str).unwrap_or("?"),
            args.get("account").and_then(Value::as_str).unwrap_or("?")
        ))
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let service = args.get("service").and_then(Value::as_str).unwrap_or("");
        let account = args.get("account").and_then(Value::as_str).unwrap_or("");
        let argv = match keychain_argv(service, account) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        if !self.binary.is_file() {
            return Ok(missing_binary(&self.binary));
        }
        let out = match run_cmd(&self.binary, &argv, None, CLI_TIMEOUT) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        if out.code != Some(0) {
            // stdout (the secret) is deliberately dropped from the error.
            let detail = out.stderr.trim();
            let code = if detail.to_ascii_lowercase().contains("could not be found") {
                ToolErrorCode::NotFound
            } else {
                ToolErrorCode::ExecutionError
            };
            return Ok(ToolResult::err(
                code,
                format!("security find-generic-password failed (exit {}): {detail}", out.code.unwrap_or(-1)),
            ));
        }
        let secret = out.stdout.strip_suffix('\n').unwrap_or(&out.stdout).to_string();
        Ok(ToolResult::ok(json!({
            "ok": true,
            "service": service,
            "account": account,
            "secret": secret,
        })))
    }
}

// ---------------------------------------------------------------------------
// defaults_write
// ---------------------------------------------------------------------------

fn defaults_argv(args: &Value) -> std::result::Result<Vec<String>, String> {
    let domain = args.get("domain").and_then(Value::as_str).unwrap_or("");
    let key = args.get("key").and_then(Value::as_str).unwrap_or("");
    let value = args.get("value").and_then(Value::as_str).unwrap_or("");
    let kind = args.get("type").and_then(Value::as_str).unwrap_or("");
    if domain.is_empty()
        || !domain.contains('.')
        || !domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        || domain.split('.').any(|c| c.is_empty())
    {
        return Err("domain must be a preference id like ru.example.app (letters, digits, dot, dash, underscore)".into());
    }
    safe_field("key", key)?;
    if !TYPES.contains(&kind) {
        return Err(format!("type must be one of {}", TYPES.join(", ")));
    }
    // int/float values are pinned by parsing (a leading "-" is a legal
    // number sign and defaults consumes it as the value after the type
    // flag); string/bool go through the strict field check instead.
    match kind {
        "int" => {
            value
                .parse::<i64>()
                .map_err(|_| format!("value {value:?} is not a valid int"))?;
        }
        "float" => {
            value
                .parse::<f64>()
                .map_err(|_| format!("value {value:?} is not a valid float"))?;
        }
        "bool" => {
            if value != "true" && value != "false" {
                return Err("bool value must be \"true\" or \"false\"".into());
            }
        }
        _ => safe_field("value", value)?,
    }
    let flag = format!("-{kind}");
    Ok(vec!["write".into(), domain.into(), key.into(), flag, value.into()])
}

pub struct DefaultsWriteTool {
    binary: PathBuf,
}

impl Default for DefaultsWriteTool {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(DEFAULTS_BINARY),
        }
    }
}

impl DefaultsWriteTool {
    #[cfg(test)]
    fn with_binary(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl Tool for DefaultsWriteTool {
    fn name(&self) -> &str {
        "defaults_write"
    }
    fn description(&self) -> &str {
        "Write one macOS preference via /usr/bin/defaults (always confirmed: it rewrites user preferences).\ndomain (e.g. ru.example.app), key, value as a string typed by \"type\": string|int|bool|float (int/float/bool values are parsed strictly). Returns {ok, domain, key, type}; use defaults read via bash to inspect."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "domain": { "type": "string" },
                "key": { "type": "string" },
                "value": { "type": "string" },
                "type": { "type": "string", "enum": ["string", "int", "bool", "float"] }
            },
            "required": ["domain", "key", "value", "type"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some(format!(
            "изменение настроек macOS: defaults write {} {}",
            args.get("domain").and_then(Value::as_str).unwrap_or("?"),
            args.get("key").and_then(Value::as_str).unwrap_or("?")
        ))
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let argv = match defaults_argv(args) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        if !self.binary.is_file() {
            return Ok(missing_binary(&self.binary));
        }
        let out = match run_cmd(&self.binary, &argv, None, CLI_TIMEOUT) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        if out.code != Some(0) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "defaults write failed (exit {}): {}",
                    out.code.unwrap_or(-1),
                    out.stderr.trim()
                ),
            ));
        }
        Ok(ToolResult::ok(json!({
            "ok": true,
            "domain": args["domain"],
            "key": args["key"],
            "type": args["type"],
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        let script = dir.join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn keychain_argv_is_validated_and_exact() {
        assert_eq!(
            keychain_argv("ru.example.app", "user@example.ru").unwrap(),
            ["find-generic-password", "-w", "-s", "ru.example.app", "-a", "user@example.ru"]
                .map(str::to_string)
                .to_vec()
        );
        assert!(keychain_argv("", "a").unwrap_err().contains("service"));
        assert!(keychain_argv("s", "  ").unwrap_err().contains("account"));
        assert!(keychain_argv("-s", "a").unwrap_err().contains("must not start with '-'"));
        assert!(keychain_argv("s", "a\nb").unwrap_err().contains("control"));
    }

    #[test]
    fn keychain_reads_the_secret_via_the_fake_security_and_never_leaks_it_in_errors() {
        let dir = temp_test_dir("kc-fake");
        let capture = dir.join("argv.txt");
        let fake = fake_bin(
            &dir,
            "security",
            &format!("printf '%s\\n' \"$@\" > '{}'\nprintf 's3cr3t\\n'", capture.display()),
        );
        let r = KeychainGetTool::with_binary(fake)
            .execute(&json!({ "service": "ru.example.app", "account": "ftp" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["service"], "ru.example.app");
        assert_eq!(data["account"], "ftp");
        assert_eq!(data["secret"], "s3cr3t");
        assert_eq!(
            fs::read_to_string(&capture).unwrap().lines().collect::<Vec<_>>(),
            ["find-generic-password", "-w", "-s", "ru.example.app", "-a", "ftp"]
        );

        // missing item: not_found, the secret (stdout) never reaches the error
        let missing = fake_bin(
            &dir,
            "security-miss",
            "echo 'The specified item could not be found in the keychain.' >&2\nprintf 'SHOULD-NOT-LEAK'\nexit 44",
        );
        let r = KeychainGetTool::with_binary(missing)
            .execute(&json!({ "service": "s", "account": "a" }), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("could not be found"), "{}", err.message);
        assert!(!err.message.contains("SHOULD-NOT-LEAK"), "{}", err.message);

        let failing = fake_bin(&dir, "security-fail", "echo 'denied' >&2\nexit 1");
        let r = KeychainGetTool::with_binary(failing)
            .execute(&json!({ "service": "s", "account": "a" }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.error.unwrap().code, "execution_error");

        let r = KeychainGetTool::with_binary(dir.join("no-security"))
            .execute(&json!({ "service": "s", "account": "a" }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");

        let r = KeychainGetTool::default()
            .execute(&json!({ "service": "", "account": "a" }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn defaults_argv_types_values_and_rejects_bad_input() {
        assert_eq!(
            defaults_argv(&json!({ "domain": "ru.example.app", "key": "mode", "value": "dark", "type": "string" })).unwrap(),
            ["write", "ru.example.app", "mode", "-string", "dark"].map(str::to_string).to_vec()
        );
        assert_eq!(
            defaults_argv(&json!({ "domain": "ru.example.app", "key": "n", "value": "-42", "type": "int" })).unwrap()[3..],
            ["-int", "-42"].map(str::to_string).to_vec()
        );
        assert_eq!(
            defaults_argv(&json!({ "domain": "ru.example.app", "key": "b", "value": "true", "type": "bool" })).unwrap()[3..],
            ["-bool", "true"].map(str::to_string).to_vec()
        );
        assert_eq!(
            defaults_argv(&json!({ "domain": "ru.example.app", "key": "f", "value": "1.5", "type": "float" })).unwrap()[3..],
            ["-float", "1.5"].map(str::to_string).to_vec()
        );

        assert!(defaults_argv(&json!({ "domain": "nodots", "key": "k", "value": "v", "type": "string" }))
            .unwrap_err()
            .contains("domain"));
        assert!(defaults_argv(&json!({ "domain": "ru.example; rm -rf /", "key": "k", "value": "v", "type": "string" }))
            .unwrap_err()
            .contains("domain"));
        assert!(defaults_argv(&json!({ "domain": "ru..example", "key": "k", "value": "v", "type": "string" }))
            .unwrap_err()
            .contains("domain"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "-k", "value": "v", "type": "string" }))
            .unwrap_err()
            .contains("key"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "", "type": "string" }))
            .unwrap_err()
            .contains("value"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "a\nb", "type": "string" }))
            .unwrap_err()
            .contains("control"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "x", "type": "int" }))
            .unwrap_err()
            .contains("int"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "1.5", "type": "int" }))
            .unwrap_err()
            .contains("int"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "yes", "type": "bool" }))
            .unwrap_err()
            .contains("true"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "abc", "type": "float" }))
            .unwrap_err()
            .contains("float"));
        assert!(defaults_argv(&json!({ "domain": "ru.example", "key": "k", "value": "v", "type": "blob" }))
            .unwrap_err()
            .contains("string, int, bool, float"));
    }

    #[test]
    fn defaults_writes_exact_argv_through_the_fake_binary() {
        let dir = temp_test_dir("dw-fake");
        let capture = dir.join("argv.txt");
        let fake = fake_bin(&dir, "defaults", &format!("printf '%s\\n' \"$@\" > '{}'", capture.display()));
        let r = DefaultsWriteTool::with_binary(fake)
            .execute(
                &json!({ "domain": "ru.example.app", "key": "mode", "value": "dark", "type": "string" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["domain"], "ru.example.app");
        assert_eq!(data["key"], "mode");
        assert_eq!(data["type"], "string");
        assert_eq!(
            fs::read_to_string(&capture).unwrap().lines().collect::<Vec<_>>(),
            ["write", "ru.example.app", "mode", "-string", "dark"]
        );

        let failing = fake_bin(&dir, "defaults-fail", "echo 'bad domain' >&2\nexit 1");
        let r = DefaultsWriteTool::with_binary(failing)
            .execute(
                &json!({ "domain": "ru.example.app", "key": "k", "value": "v", "type": "string" }),
                &ToolCtx::default(),
            )
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("bad domain"), "{}", err.message);

        let r = DefaultsWriteTool::with_binary(dir.join("no-defaults"))
            .execute(
                &json!({ "domain": "ru.example.app", "key": "k", "value": "v", "type": "string" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn schemas_and_policies_match_the_spec() {
        let kc = KeychainGetTool::default();
        assert_eq!(kc.class(), ToolClass::ReadOnly);
        assert_eq!(kc.policy(), PermissionPolicy::Check);
        assert!(kc.danger_reason(&json!({ "service": "s", "account": "a" }), &ToolCtx::default())
            .unwrap()
            .contains("Keychain"));
        assert!(validate_args(&kc.parameters(), &json!({ "service": "s", "account": "a" })).is_ok());
        assert!(validate_args(&kc.parameters(), &json!({ "service": "s" })).unwrap_err().contains("account"));
        assert!(validate_args(&kc.parameters(), &json!({ "service": "s", "account": "a", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));

        let dw = DefaultsWriteTool::default();
        assert_eq!(dw.class(), ToolClass::Mutating);
        assert_eq!(dw.policy(), PermissionPolicy::Check);
        assert!(dw.danger_reason(
            &json!({ "domain": "ru.example.app", "key": "k", "value": "v", "type": "string" }),
            &ToolCtx::default()
        )
        .unwrap()
        .contains("defaults write"));
        assert!(validate_args(&dw.parameters(), &json!({ "domain": "d.e", "key": "k", "value": "v", "type": "bool" })).is_ok());
        assert!(validate_args(&dw.parameters(), &json!({ "domain": "d.e", "key": "k", "value": "v" }))
            .unwrap_err()
            .contains("type"));
        assert!(validate_args(&dw.parameters(), &json!({ "domain": "d.e", "key": "k", "value": "v", "type": "blob" }))
            .unwrap_err()
            .contains("enum"));
    }
}
