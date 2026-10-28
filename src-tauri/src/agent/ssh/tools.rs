// Tool definitions: ssh_list_profiles, ssh_exec, ssh_upload/ssh_download,
// ssh_generate_key, ssh_install_key, ssh_save_profile_to_datas. Names, JSON
// schemas, classes, permission policies, danger reasons and result envelopes
// are unchanged from the original single-file module.
use super::askpass::askpass_for;
use super::command::{
    check_remote_path, clamp_timeout, default_key_path, expand_tilde, install_key_command,
    resolve_local_path, resolve_target, shorten, ssh_exec_argv, scp_argv, target_error,
    target_label, valid_host_or_user, ExecOpts, SSH_TIMEOUT,
};
use super::config::{all_profiles, auth_kind, datas_section_title};
use super::policy::{destructive_reasons, is_read_only_command};
use super::session::{run_process, SSH_RESULT_CAP};
use crate::agent::harness_tools::resolve_path;
use crate::agent::project_tools::{envelope, require_wd, with_conn};
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::task_glue::{read_datas, upsert_section, write_datas};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::netdiag;
use serde_json::{json, Value};
use std::fs;
use std::time::Duration;

// A failed connect usually means the host is unreachable from *this* process
// (macOS local network permission denied, or a TUN client left reject routes
// behind) rather than that the server is down. Build one compact snapshot and
// an actionable hint instead of leaving the agent to guess from stderr.
pub(super) fn net_diagnostics(code: i32, stderr: &str, host: &str) -> Option<Value> {
    (code != 0 && netdiag::looks_unreachable(stderr)).then(|| netdiag::snapshot(Some(host)))
}

pub struct SshListProfilesTool;

impl Tool for SshListProfilesTool {
    fn name(&self) -> &str {
        "ssh_list_profiles"
    }
    fn description(&self) -> &str {
        "List SSH profiles merged from Settings/config/ssh.json and the current project's datas.md (\"## SSH <name>\" sections; hand-written sections with host/user fields under any heading, Russian labels (Хост:, Пользователь:, Порт:, Пароль:, Ключ:) and bullet lists are recognized too; a datas entry replaces a config entry of the same name): name, host, user, port, source, auth (\"password\" | \"key\" | \"key+password\") and hasPassword. Passwords, key paths and key material are never returned; a profile with neither an explicit key nor a password still authenticates via ssh-agent (auth \"key\")."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let profiles = match all_profiles(ctx) {
            Ok(p) => p,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let profiles: Vec<Value> = profiles
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "host": p.host,
                    "user": p.user,
                    "port": p.port,
                    "hasPassword": p.password.is_some(),
                    "auth": auth_kind(p),
                    "source": p.source,
                })
            })
            .collect();
        Ok(ToolResult::ok(json!({ "profiles": profiles })))
    }
}

pub struct SshExecTool;

impl Tool for SshExecTool {
    fn name(&self) -> &str {
        "ssh_exec"
    }
    fn description(&self) -> &str {
        "Run a command on a remote host via the system ssh CLI (host keys follow accept-new by default). Target: a named profile from Settings/config/ssh.json or the current project's datas.md (the profile wins over explicit fields and carries its own key/password, so prefer profiles) or explicit host/user/port/keyPath/password. Key-only profiles run with BatchMode=yes (keys/ssh-agent); a profile or explicit password authenticates through SSH_ASKPASS (the secret never appears in argv, results or errors; exactly one prompt). With a key and a password, publickey is tried first. Each call is a fresh non-interactive session: nothing persists between calls, so keep commands self-contained and drive package managers non-interactively (apt-get -y, DEBIAN_FRONTEND=noninteractive, sudo -n). By default the command runs as `bash -lc '<command>'` so PATH/profile apply; set loginShell=false to run it verbatim. Optional cwd runs it inside that remote directory. pty=true allocates a TTY (-tt) for programs that need one, but interactive prompts then hang until the timeout. timeoutSec defaults to 120 and is clamped to 10..1800; long installs should raise it or run detached (nohup ... > log 2>&1 &) and poll the log. Returns exitCode, stdout, stderr; output capped."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "keyPath": { "type": "string" },
                "password": { "type": "string" },
                "command": { "type": "string" },
                "timeoutSec": { "type": "integer" },
                "loginShell": { "type": "boolean" },
                "cwd": { "type": "string" },
                "pty": { "type": "boolean" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if is_read_only_command(command) {
            return None;
        }
        let label = target_label(args, ctx);
        let mut reason = format!("выполнение команды по SSH на {label}");
        let flags = destructive_reasons(command);
        if !flags.is_empty() {
            reason.push_str(&format!("; деструктивные признаки: {}", flags.join("; ")));
        }
        Some(reason)
    }
    fn max_result_len(&self) -> usize {
        SSH_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args, ctx) {
            Ok(t) => t,
            Err(e) => return Ok(target_error(e)),
        };
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if command.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "command is empty"));
        }
        let opts = ExecOpts {
            login_shell: args
                .get("loginShell")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            cwd: args
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
            pty: args.get("pty").and_then(Value::as_bool).unwrap_or(false),
            timeout: clamp_timeout(args.get("timeoutSec").and_then(Value::as_i64)),
        };
        let argv = ssh_exec_argv(&target, command, &opts);
        let askpass = askpass_for(&target, &wd)?;
        let envs = askpass.as_ref().map(|a| a.envs()).unwrap_or_default();
        let out = run_process("ssh", &argv, &wd, opts.timeout, &envs, ctx.cancel.as_deref())?;
        let diagnostics = net_diagnostics(out.code, &out.stderr, &target.host);
        let mut data = json!({
            "target": target.label(),
            "exitCode": out.code,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        });
        if let Some(diagnostics) = diagnostics {
            data["hint"] = json!(netdiag::HINT);
            data["diagnostics"] = diagnostics;
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

pub struct SshCopyTool {
    upload: bool,
}

impl SshCopyTool {
    pub fn upload() -> Self {
        Self { upload: true }
    }
    pub fn download() -> Self {
        Self { upload: false }
    }
}

impl Tool for SshCopyTool {
    fn name(&self) -> &str {
        if self.upload {
            "ssh_upload"
        } else {
            "ssh_download"
        }
    }
    fn description(&self) -> &str {
        if self.upload {
            "Copy a local file or folder to the remote host via the system scp CLI (recursive=true adds -r). localPath is project-relative inside the project folder (project agent) or a resolved path (harness agent); remotePath is passed through to scp unchanged. Target and auth resolution is the same as ssh_exec (profiles from ssh.json and the current project's datas.md, key or askpass password auth)."
        } else {
            "Copy a remote file or folder to the local disk via the system scp CLI (recursive=true adds -r). localPath is the landing path (project-contained for the project agent, resolved for the harness agent); remotePath is passed through to scp unchanged. Target and auth resolution is the same as ssh_exec (profiles from ssh.json and the current project's datas.md, key or askpass password auth)."
        }
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "keyPath": { "type": "string" },
                "password": { "type": "string" },
                "localPath": { "type": "string" },
                "remotePath": { "type": "string" },
                "recursive": { "type": "boolean" }
            },
            "required": ["localPath", "remotePath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let label = target_label(args, ctx);
        Some(if self.upload {
            format!("копирование локального файла на SSH-хост {label}")
        } else {
            format!("копирование файла с SSH-хоста {label} на локальный диск")
        })
    }
    fn max_result_len(&self) -> usize {
        SSH_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args, ctx) {
            Ok(t) => t,
            Err(e) => return Ok(target_error(e)),
        };
        let remote = args.get("remotePath").and_then(Value::as_str).unwrap_or("");
        if let Err(e) = check_remote_path(remote) {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string()));
        }
        let local_raw = args.get("localPath").and_then(Value::as_str).unwrap_or("");
        let local = match resolve_local_path(local_raw, ctx, &wd, self.upload) {
            Ok(p) => p,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let recursive = args.get("recursive").and_then(Value::as_bool).unwrap_or(false);
        let (src, dst) = if self.upload {
            (local.display().to_string(), remote.to_string())
        } else {
            (remote.to_string(), local.display().to_string())
        };
        let argv = scp_argv(&target, &src, &dst, recursive);
        let askpass = askpass_for(&target, &wd)?;
        let envs = askpass.as_ref().map(|a| a.envs()).unwrap_or_default();
        let out = run_process("scp", &argv, &wd, SSH_TIMEOUT, &envs, ctx.cancel.as_deref())?;
        let diagnostics = net_diagnostics(out.code, &out.stderr, &target.host);
        let mut data = json!({
            "target": target.label(),
            "exitCode": out.code,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        });
        if let Some(diagnostics) = diagnostics {
            data["hint"] = json!(netdiag::HINT);
            data["diagnostics"] = diagnostics;
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

// ---- key generation and installation ----

pub struct SshGenerateKeyTool;

impl Tool for SshGenerateKeyTool {
    fn name(&self) -> &str {
        "ssh_generate_key"
    }
    fn description(&self) -> &str {
        "Generate a new ed25519 SSH keypair with the system ssh-keygen CLI (`ssh-keygen -t ed25519 -N '' -f <path>`, optional -C comment). path is a resolved path (~/wd-relative/absolute); it defaults to .ssh/id_ed25519 inside the current project folder (project agent) or Harness/tmp/ssh/id_ed25519 (harness agent). An existing key file is never overwritten. Returns privateKeyPath and publicKey (the .pub line); install the public key on a host with ssh_install_key and reference the private key as keyPath in profiles or calls."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "comment": { "type": "string" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let where_ = match (
            args.get("path").and_then(Value::as_str),
            ctx.working_dir.as_deref(),
        ) {
            (Some(raw), Some(wd)) if !raw.trim().is_empty() => shorten(&resolve_path(raw.trim(), wd)),
            (_, Some(wd)) => match default_key_path(ctx, wd) {
                Ok(p) => shorten(&p),
                Err(_) => "папке по умолчанию".to_string(),
            },
            _ => "папке по умолчанию".to_string(),
        };
        Some(format!("генерация SSH-ключа ed25519 в {where_}"))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("").trim();
        let path = if raw.is_empty() {
            default_key_path(ctx, &wd)?
        } else {
            resolve_path(raw, &wd)
        };
        if path.exists() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("refusing to overwrite existing key file: {}", shorten(&path)),
            ));
        }
        let comment = args.get("comment").and_then(Value::as_str).unwrap_or("").trim();
        if comment.chars().any(char::is_control) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "comment must not contain control characters",
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut argv = vec![
            "-t".to_string(),
            "ed25519".to_string(),
            "-N".to_string(),
            String::new(),
            "-f".to_string(),
            path.display().to_string(),
        ];
        if !comment.is_empty() {
            argv.push("-C".to_string());
            argv.push(comment.to_string());
        }
        let out = run_process("ssh-keygen", &argv, &wd, Duration::from_secs(30), &[], ctx.cancel.as_deref())?;
        if out.code != 0 {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("ssh-keygen failed: {}", out.stderr.trim()),
            ));
        }
        let public = match fs::read_to_string(format!("{}.pub", path.display())) {
            Ok(text) => text.trim().to_string(),
            Err(_) => String::new(),
        };
        Ok(ToolResult::ok(json!({
            "privateKeyPath": path.display().to_string(),
            "publicKey": public,
        })))
    }
}

pub struct SshInstallKeyTool;

impl Tool for SshInstallKeyTool {
    fn name(&self) -> &str {
        "ssh_install_key"
    }
    fn description(&self) -> &str {
        "Install a local SSH public key into the remote account's authorized_keys (idempotent): reads <keyPath>.pub and runs `mkdir -p ~/.ssh; chmod 700 ~/.ssh; grep -qxF '<pub>' ~/.ssh/authorized_keys || echo '<pub>' >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys` in one session. Target and auth resolution is the same as ssh_exec: a profile (ssh.json or the current project's datas.md) or explicit host/user/port plus password (askpass) and/or keyPath. Typical flow: ssh_generate_key, then ssh_install_key with the existing password, then use the key in profiles. Returns exitCode and installed=true on success."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "password": { "type": "string" },
                "keyPath": { "type": "string" }
            },
            "required": ["keyPath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let label = target_label(args, ctx);
        Some(format!("установка SSH-ключа в authorized_keys на хосте {label}"))
    }
    fn max_result_len(&self) -> usize {
        SSH_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args, ctx) {
            Ok(t) => t,
            Err(e) => return Ok(target_error(e)),
        };
        let key_path = args.get("keyPath").and_then(Value::as_str).unwrap_or("").trim();
        if key_path.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "keyPath is empty"));
        }
        let pub_path = format!("{}.pub", expand_tilde(key_path).display());
        let public = match fs::read_to_string(&pub_path) {
            Ok(text) => text.trim().to_string(),
            Err(_) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::NotFound,
                    format!("public key file not found: {pub_path} (generate a key first)"),
                ))
            }
        };
        if public.is_empty() || public.contains(['\n', '\r']) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "public key must be a single line",
            ));
        }
        let command = install_key_command(&public);
        let opts = ExecOpts {
            login_shell: false,
            cwd: None,
            pty: false,
            timeout: SSH_TIMEOUT,
        };
        let argv = ssh_exec_argv(&target, &command, &opts);
        let askpass = askpass_for(&target, &wd)?;
        let envs = askpass.as_ref().map(|a| a.envs()).unwrap_or_default();
        let out = run_process("ssh", &argv, &wd, SSH_TIMEOUT, &envs, ctx.cancel.as_deref())?;
        let diagnostics = net_diagnostics(out.code, &out.stderr, &target.host);
        let mut data = json!({
            "target": target.label(),
            "exitCode": out.code,
            "installed": out.code == 0,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        });
        if let Some(diagnostics) = diagnostics {
            data["hint"] = json!(netdiag::HINT);
            data["diagnostics"] = diagnostics;
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

// ---- profile persistence in datas.md ----

pub struct SshSaveProfileToDatasTool;

impl Tool for SshSaveProfileToDatasTool {
    fn name(&self) -> &str {
        "ssh_save_profile_to_datas"
    }
    fn description(&self) -> &str {
        "Save (add or update) an SSH profile as a \"## SSH <name>\" section in the current project's datas.md — the user's chosen storage for access data: host:, user:, port:, optional password: and key: lines. An existing section with the same name (any heading case) is replaced in place, otherwise appended; datas.md is created when missing. The password is written to the file but never echoed back by this or any other SSH tool; ssh_list_profiles only reports hasPassword/auth. The saved profile is immediately usable as \"profile\": \"<name>\" by ssh_exec/ssh_upload/ssh_download."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "password": { "type": "string" },
                "keyPath": { "type": "string" }
            },
            "required": ["name", "host", "user", "port"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = ctx
            .project_id
            .ok_or_else(|| AppError::Invalid("no project bound to this agent run".into()))?;
        let wd = require_wd(ctx)?;
        let name = args.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if name.is_empty() || name.starts_with('#') || name.contains(['\n', '\r']) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "name must be a single plain line without '#'",
            ));
        }
        let host = args.get("host").and_then(Value::as_str).unwrap_or("").trim();
        let user = args.get("user").and_then(Value::as_str).unwrap_or("").trim();
        if !valid_host_or_user(host) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("invalid ssh host {host:?}"),
            ));
        }
        if !valid_host_or_user(user) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("invalid ssh user {user:?}"),
            ));
        }
        let port = args.get("port").and_then(Value::as_i64).unwrap_or(0);
        if !(1..=65535).contains(&port) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("ssh port {port} is out of range 1-65535"),
            ));
        }
        let mut body = format!("host: {host}\nuser: {user}\nport: {port}");
        if let Some(pw) = args.get("password").and_then(Value::as_str) {
            if !pw.trim().is_empty() {
                body.push_str(&format!("\npassword: {}", pw.trim()));
            }
        }
        if let Some(k) = args.get("keyPath").and_then(Value::as_str) {
            if !k.trim().is_empty() {
                body.push_str(&format!("\nkey: {}", k.trim()));
            }
        }
        let r = with_conn(ctx, |conn| {
            let current = read_datas(conn, &wd, pid)?;
            let title = datas_section_title(&current, &name);
            let (updated, new_md) = upsert_section(&current, &title, &body);
            write_datas(conn, &wd, pid, &new_md).map(|_| updated)
        });
        Ok(match r {
            Ok(updated) => ToolResult::ok(json!({ "name": name, "updated": updated, "source": "datas" })),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}
