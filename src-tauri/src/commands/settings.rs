use crate::{config, paths};
use crate::error::{Result};
use crate::state::{AppState};
use serde_json::Value;
use tauri::{State};

#[tauri::command]
pub(crate) fn ping() -> String {
    "pong".into()
}

#[tauri::command]
pub(crate) fn config_dir_path(state: State<AppState>) -> Result<String> {
    Ok(paths::config_dir(&state.working_dir()?).display().to_string())
}

#[tauri::command]
pub(crate) fn read_config(state: State<AppState>, kind: config::ConfigKind) -> Result<Value> {
    config::read_config(&state.working_dir()?, kind)
}

#[tauri::command]
pub(crate) fn write_config(state: State<AppState>, kind: config::ConfigKind, value: Value) -> Result<()> {
    config::write_config(&state.working_dir()?, kind, value)
}
