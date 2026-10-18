use crate::{mcp};
use crate::error::{Result};
use crate::state::{AppState};
use crate::sync_util::lock_recover;
use tauri::{State};

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
    let mut registry = lock_recover(&state.mcp);
    registry.shutdown();
    *registry = mcp::McpRegistry::start(&wd);
    Ok(registry.status())
}
