use crate::{config, mcp};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use crate::sync_util::lock_recover;
use super::emit_config_changed;
use serde_json::{Value, json};
use tauri::{AppHandle, State};

// Per-server MCP status for the settings UI (SPEC 6.2; the UI itself is a
// later slice).
#[tauri::command]
pub(crate) fn mcp_status(state: State<AppState>) -> Vec<mcp::McpServerStatus> {
    lock_recover(&state.mcp).status()
}

// Re-reads mcp.json and restarts the servers; a server that fails to start
// keeps its error in the status rows instead of breaking the app.
#[tauri::command]
pub(crate) fn mcp_reload(state: State<AppState>) -> Result<Vec<mcp::McpServerStatus>> {
    let wd = state.working_dir()?;
    Ok(restart_registry(state, &wd))
}

// Persists the enabled flag of one server in mcp.json (unknown keys survive),
// then restarts the registry: off kills the child process/connection and
// drops its tools from the agent, on spawns/connects it again. Emits the same
// config-changed event as the file watcher so open UIs refresh.
#[tauri::command]
pub(crate) fn mcp_set_enabled(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    enabled: bool,
) -> Result<Vec<mcp::McpServerStatus>> {
    let wd = state.working_dir()?;
    let mut raw = config::read_config(&wd, config::ConfigKind::Mcp)?;
    let updated = set_enabled_in(&raw, &name, enabled)?;
    config::write_config(&wd, config::ConfigKind::Mcp, json!({ "servers": updated }))?;
    let status = restart_registry(state, &wd);
    emit_config_changed(&app, config::ConfigKind::Mcp);
    Ok(status)
}

// Restarts the live MCP registry from mcp.json and returns fresh status rows.
fn restart_registry(state: State<'_, AppState>, wd: &std::path::Path) -> Vec<mcp::McpServerStatus> {
    let mut registry = lock_recover(&state.mcp);
    registry.shutdown();
    *registry = mcp::McpRegistry::start(wd);
    registry.status()
}

// Pure patch of the servers array: flips `enabled` on the entry with this
// name, leaving every other key untouched; error when the server is unknown.
fn set_enabled_in(raw: &Value, name: &str, enabled: bool) -> Result<Vec<Value>> {
    let servers = match raw.get("servers") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(servers)) => servers.clone(),
        Some(_) => return Err(AppError::Invalid("mcp.json servers must be an array".into())),
    };
    let mut found = false;
    let servers = servers
        .into_iter()
        .map(|mut server| {
            if server.get("name").and_then(Value::as_str) == Some(name) {
                found = true;
                server["enabled"] = json!(enabled);
            }
            server
        })
        .collect();
    if !found {
        return Err(AppError::Invalid(format!("server \"{name}\" not found in mcp.json")));
    }
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_enabled_in_flips_only_the_flag_and_keeps_other_keys() {
        let raw = json!({ "servers": [
            { "name": "one", "enabled": true, "handAdded": 7,
              "transport": { "type": "stdio", "command": "sh", "args": ["-c", "x"] } },
            { "name": "two", "enabled": true,
              "transport": { "type": "http", "url": "https://mcp.example.com/mcp" } },
        ] });
        let servers = set_enabled_in(&raw, "one", false).unwrap();
        assert_eq!(servers[0]["enabled"], json!(false));
        assert_eq!(servers[0]["handAdded"], json!(7));
        assert_eq!(servers[0]["transport"]["command"], json!("sh"));
        assert_eq!(servers[1]["enabled"], json!(true));
        let back = set_enabled_in(&json!({ "servers": servers }), "one", true).unwrap();
        assert_eq!(back[0]["enabled"], json!(true));
    }

    #[test]
    fn set_enabled_in_rejects_unknown_server_and_broken_shape() {
        let raw = json!({ "servers": [{ "name": "one", "enabled": true,
            "transport": { "type": "stdio", "command": "sh" } }] });
        let err = set_enabled_in(&raw, "ghost", true).unwrap_err().to_string();
        assert!(err.contains("ghost"), "{err}");
        assert!(set_enabled_in(&json!({ "servers": "nope" }), "one", true).is_err());
        assert!(set_enabled_in(&json!({}), "one", true).is_err());
    }
}
