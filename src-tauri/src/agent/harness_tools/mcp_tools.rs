// The `mcp_add` harness tool (SPEC 6.2 "Добавление сервера из чата"): the
// agent adds an MCP server from a user instruction. The write goes through
// the SAME config path as the settings UI (config::read_config/write_config
// on Settings/config/mcp.json — no parallel store), then the live MCP
// registry is restarted exactly like the mcp_reload command so the status
// rows and bridged tools refresh at once. Validation mirrors the UI
// (name shape, transport fields, SSRF denylist); duplicate names are
// refused. Mutating + Check: adding a server can spawn an arbitrary local
// process, so every call goes through the shared confirmation queue.
use crate::agent::project_tools::state_of;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::config::{self, ConfigKind};
use crate::error::{AppError, Result};
use crate::mcp;
use crate::sync_util::lock_recover;
use serde_json::{json, Value};
use std::collections::BTreeMap;

// Same shape the settings UI enforces: latin letters, digits, hyphen and
// underscore, starting alphanumeric; the name also seeds the
// mcp_<server>__<tool> prefix, so it stays catalog-safe.
fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn string_map(args: &Value, key: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    match args.get(key) {
        None | Some(Value::Null) => return Ok(out),
        Some(Value::Object(map)) => {
            for (k, v) in map {
                let value = v.as_str().ok_or_else(|| {
                    AppError::Invalid(format!("{key}.{k} must be a string"))
                })?;
                if k.is_empty() {
                    return Err(AppError::Invalid(format!("{key} keys must not be empty")));
                }
                out.insert(k.clone(), value.to_string());
            }
        }
        Some(_) => return Err(AppError::Invalid(format!("{key} must be an object"))),
    }
    Ok(out)
}

// Builds and validates the transport object from tool args, mirroring the
// UI form rules: stdio needs a command (args/env optional), http/sse need a
// plain http(s) url that passes the SSRF denylist (headers optional).
fn build_transport(args: &Value, transport: &str) -> Result<Value> {
    match transport {
        "stdio" => {
            let command = args["command"].as_str().unwrap_or("").trim();
            if command.is_empty() {
                return Err(AppError::Invalid(
                    "stdio transport needs a command (npx, node, python…)".into(),
                ));
            }
            let mut entry = json!({ "type": "stdio", "command": command });
            match args.get("args") {
                None | Some(Value::Null) => entry["args"] = json!([]),
                Some(list @ Value::Array(_)) => {
                    let mut items = Vec::new();
                    for item in list.as_array().unwrap() {
                        let text = item
                            .as_str()
                            .ok_or_else(|| AppError::Invalid("args must be strings".into()))?;
                        if !text.is_empty() {
                            items.push(text.to_string());
                        }
                    }
                    entry["args"] = json!(items);
                }
                Some(_) => return Err(AppError::Invalid("args must be an array of strings".into())),
            }
            let env = string_map(args, "env")?;
            if !env.is_empty() {
                entry["env"] = serde_json::to_value(env).unwrap();
            }
            Ok(entry)
        }
        kind @ ("http" | "sse") => {
            let url = args["url"].as_str().unwrap_or("").trim();
            if url.is_empty() {
                return Err(AppError::Invalid("http/sse transport needs a url".into()));
            }
            mcp::check_url(url)?;
            let mut entry = json!({ "type": kind, "url": url });
            let headers = string_map(args, "headers")?;
            if !headers.is_empty() {
                entry["headers"] = serde_json::to_value(headers).unwrap();
            }
            Ok(entry)
        }
        other => Err(AppError::Invalid(format!(
            "unknown transport \"{other}\" (stdio|http|sse)"
        ))),
    }
}

pub struct McpAddTool;

impl Tool for McpAddTool {
    fn name(&self) -> &str {
        "mcp_add"
    }
    fn description(&self) -> &str {
        "Add an MCP server to Settings/config/mcp.json (the same file the settings UI edits) and restart the MCP registry so its tools appear. transport stdio: command + optional args/env; http/sse: url + optional headers. Name: latin letters, digits, hyphen, underscore. Fails on a duplicate name or a refused url. Confirmed before writing."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "transport": { "type": "string", "enum": ["stdio", "http", "sse"] },
                "command": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "env": { "type": "object", "additionalProperties": { "type": "string" } },
                "url": { "type": "string" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "enabled": { "type": "boolean" }
            },
            "required": ["name", "transport"],
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
        let name = args["name"].as_str().unwrap_or("?");
        let detail = match args["transport"].as_str() {
            Some("stdio") => format!(
                "stdio: запуск команды «{}»",
                args["command"].as_str().unwrap_or("?")
            ),
            Some(kind @ ("http" | "sse")) => {
                format!("{kind}: подключение к «{}»", args["url"].as_str().unwrap_or("?"))
            }
            _ => "транспорт не указан".to_string(),
        };
        Some(format!(
            "добавление MCP-сервера «{name}» в mcp.json ({detail})"
        ))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let state = state_of(ctx)?;
        let name = args["name"].as_str().unwrap_or("").trim();
        if !valid_server_name(name) {
            return Ok(invalid(
                "invalid server name: use latin letters, digits, hyphen or underscore",
            ));
        }
        let Some(transport_kind) = args["transport"].as_str() else {
            return Ok(invalid("transport must be stdio, http or sse"));
        };
        let transport = match build_transport(args, transport_kind) {
            Ok(transport) => transport,
            Err(e) => return Ok(invalid(e.to_string())),
        };
        let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);

        // Read-modify-write of mcp.json through the shared config layer,
        // exactly like the settings UI: unknown keys survive.
        let mut raw = match config::read_config(&wd, ConfigKind::Mcp) {
            Ok(raw) => raw,
            Err(e) => return Ok(execution_error(format!("cannot read mcp.json: {e}"))),
        };
        let Some(root) = raw.as_object_mut() else {
            return Ok(execution_error("mcp.json must be a JSON object".into()));
        };
        let servers = match root.get("servers") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(servers)) => servers.clone(),
            Some(_) => {
                return Ok(execution_error("mcp.json servers must be an array".into()));
            }
        };
        if servers
            .iter()
            .any(|s| s.get("name").and_then(Value::as_str) == Some(name))
        {
            return Ok(invalid(format!(
                "server \"{name}\" already exists in mcp.json"
            )));
        }
        let mut servers = servers;
        servers.push(json!({ "name": name, "enabled": enabled, "transport": transport }));
        if let Err(e) = config::write_config(&wd, ConfigKind::Mcp, json!({ "servers": servers })) {
            return Ok(execution_error(format!("cannot write mcp.json: {e}")));
        }

        // Restart the live registry the same way the mcp_reload command
        // does, so status rows and bridged tools refresh immediately.
        let status = {
            let mut registry = lock_recover(&state.mcp);
            registry.shutdown();
            *registry = mcp::McpRegistry::start(&wd);
            registry.status()
        };
        let new_status = status.iter().find(|s| s.name == name);
        Ok(ToolResult::ok(json!({
            "server": {
                "name": name,
                "enabled": enabled,
                "transport": transport_kind,
            },
            "status": new_status,
            "servers": status,
        })))
    }
}

fn invalid(message: impl Into<String>) -> ToolResult {
    ToolResult::err(ToolErrorCode::InvalidArgs, message)
}

fn execution_error(message: String) -> ToolResult {
    ToolResult::err(ToolErrorCode::ExecutionError, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::setup;
    use crate::sync_util::lock_recover;
    use serde_json::json;
    use std::fs;

    fn mcp_json(wd: &std::path::Path) -> Value {
        config::read_config(wd, ConfigKind::Mcp).unwrap()
    }

    #[test]
    fn mcp_add_writes_mcp_json_and_refreshes_registry_status() {
        let env = setup("mcp-add-write");
        // enabled:false keeps the test offline: the registry reports the
        // server without spawning anything.
        let r = McpAddTool
            .execute(
                &json!({
                    "name": "example-mcp",
                    "transport": "stdio",
                    "command": "npx",
                    "args": ["-y", "@example/mcp-server"],
                    "env": { "API_KEY": "s3cret" },
                    "enabled": false,
                }),
                &env.ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["server"]["name"], "example-mcp");
        assert_eq!(data["status"]["status"], "disabled");
        assert_eq!(data["status"]["transport"], "stdio");

        // The SAME file the settings UI reads/writes now carries the server.
        let raw = mcp_json(&env.wd);
        assert_eq!(raw["servers"][0]["name"], "example-mcp");
        assert_eq!(raw["servers"][0]["transport"]["command"], "npx");
        assert_eq!(raw["servers"][0]["transport"]["env"]["API_KEY"], "s3cret");
        assert_eq!(raw["servers"][0]["enabled"], false);

        // The live registry (mcp_status rows) sees it too — no parallel store.
        let state = env.ctx.state.as_ref().unwrap();
        let status = lock_recover(&state.mcp).status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "example-mcp");
        assert_eq!(status[0].status, "disabled");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn mcp_add_rejects_duplicates_and_invalid_input_without_writing() {
        let env = setup("mcp-add-invalid");
        let ok = json!({ "name": "one", "transport": "stdio", "command": "npx", "enabled": false });
        assert!(McpAddTool.execute(&ok, &env.ctx).unwrap().ok);

        let dup = McpAddTool
            .execute(&json!({ "name": "one", "transport": "stdio", "command": "node" }), &env.ctx)
            .unwrap();
        assert_eq!(dup.error.as_ref().unwrap().code, "invalid_args");
        assert!(dup.error.as_ref().unwrap().message.contains("already exists"));

        for bad in [
            json!({ "name": "Bad Name", "transport": "stdio", "command": "npx" }),
            json!({ "name": "-lead", "transport": "stdio", "command": "npx" }),
            json!({ "name": "", "transport": "stdio", "command": "npx" }),
            json!({ "name": "ok", "transport": "carrier-pigeon" }),
            json!({ "name": "ok", "transport": "stdio" }),
            json!({ "name": "ok", "transport": "stdio", "command": "  " }),
            json!({ "name": "ok", "transport": "http" }),
            json!({ "name": "ok", "transport": "http", "url": "ftp://mcp.example.ru/mcp" }),
            json!({ "name": "ok", "transport": "http", "url": "http://169.254.169.254/mcp" }),
            json!({ "name": "ok", "transport": "stdio", "command": "npx", "args": "not-a-list" }),
            json!({ "name": "ok", "transport": "stdio", "command": "npx", "env": "not-an-object" }),
        ] {
            let r = McpAddTool.execute(&bad, &env.ctx).unwrap();
            assert_eq!(
                r.error.as_ref().unwrap().code,
                "invalid_args",
                "must reject {bad}"
            );
        }
        // Only the one valid write landed.
        assert_eq!(mcp_json(&env.wd)["servers"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn mcp_add_keeps_unknown_keys_and_appends_servers() {
        let env = setup("mcp-add-append");
        config::write_config(
            &env.wd,
            ConfigKind::Mcp,
            json!({ "servers": [], "futureTop": true }),
        )
        .unwrap();
        fs::write(
            crate::config::file_path(&env.wd, ConfigKind::Mcp),
            r#"{ "servers": [{"name": "hand", "enabled": false,
                 "transport": { "type": "stdio", "command": "sh" }, "handAdded": 7}],
                 "futureTop": true }"#,
        )
        .unwrap();
        let r = McpAddTool
            .execute(
                &json!({ "name": "two", "transport": "sse", "url": "http://localhost:9001/sse",
                         "headers": { "Authorization": "Bearer x" }, "enabled": false }),
                &env.ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let raw = mcp_json(&env.wd);
        assert_eq!(raw["futureTop"], true);
        assert_eq!(raw["servers"][0]["handAdded"], 7);
        assert_eq!(raw["servers"][1]["name"], "two");
        assert_eq!(raw["servers"][1]["transport"]["type"], "sse");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn harness_creates_a_skill_from_a_pasted_instruction_and_lists_it() {
        // The skills half of the chat-driven slice: create_skill is already
        // registered in the harness catalog; a pasted instruction body must
        // land as a valid SKILL.md under Settings/skills/user/ and show up
        // in the same list the settings UI reads.
        use crate::skills;

        let env = setup("mcp-add-skill");
        let instruction = concat!(
            "When the user asks for a CSV report:\n",
            "1. Read the datas.md of the project.\n",
            "2. Write the table to report.csv with UTF-8 BOM.\n"
        );
        let r = crate::skills::CreateSkillTool
            .execute(
                &json!({
                    "name": "csv-report",
                    "description": "CSV report recipe from chat",
                    "body": instruction,
                }),
                &env.ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let text =
            fs::read_to_string(crate::paths::skills_user_dir(&env.wd).join("csv-report/SKILL.md"))
                .unwrap();
        assert!(text.contains("name: csv-report"));
        assert!(text.contains("UTF-8 BOM"));
        assert_eq!(skills::load_full(&env.wd, "csv-report").unwrap(), instruction.trim());
        assert!(skills::short_list(&env.wd)
            .contains(&("csv-report".to_string(), "CSV report recipe from chat".to_string())));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn mcp_add_confirms_every_call_with_a_russian_danger_reason() {
        assert_eq!(McpAddTool.class(), ToolClass::Mutating);
        assert_eq!(McpAddTool.policy(), PermissionPolicy::Check);
        let stdio = McpAddTool.danger_reason(
            &json!({ "name": "one", "transport": "stdio", "command": "npx" }),
            &ToolCtx::default(),
        );
        assert!(stdio.as_deref().unwrap().contains("MCP-сервера «one»"));
        assert!(stdio.as_deref().unwrap().contains("npx"));
        let http = McpAddTool.danger_reason(
            &json!({ "name": "remote", "transport": "http", "url": "https://mcp.example.com/mcp" }),
            &ToolCtx::default(),
        );
        assert!(http.as_deref().unwrap().contains("https://mcp.example.com/mcp"));
    }
}
