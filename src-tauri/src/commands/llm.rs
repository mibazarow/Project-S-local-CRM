use crate::{llm};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use tauri::{State};

#[tauri::command]
pub(crate) async fn list_models(state: State<'_, AppState>) -> Result<Vec<llm::ModelInfo>> {
    let wd = state.working_dir()?;
    tauri::async_runtime::spawn_blocking(move || llm::client_from_config(&wd)?.list_models())
        .await
        .map_err(|e| AppError::Io(format!("llm task: {e}")))?
}

#[tauri::command]
pub(crate) async fn test_connection(state: State<'_, AppState>) -> Result<llm::ConnectionTest> {
    let wd = state.working_dir()?;
    let client = llm::client_from_config(&wd)?;
    tauri::async_runtime::spawn_blocking(move || client.test_connection())
        .await
        .map_err(|e| AppError::Io(format!("llm task: {e}")))
}

// Probes whether the configured model accepts image content parts (a manual
// `vision` override in llm.json short-circuits the probe) and caches the
// result in `visionDetected`. The UI warns the user from the returned status.
#[tauri::command]
pub(crate) async fn detect_vision(state: State<'_, AppState>) -> Result<llm::VisionStatus> {
    let wd = state.working_dir()?;
    tauri::async_runtime::spawn_blocking(move || llm::detect_vision(&wd))
        .await
        .map_err(|e| AppError::Io(format!("vision probe task: {e}")))?
}

// Cached/overridden vision status without any network call.
#[tauri::command]
pub(crate) fn get_vision_status(state: State<AppState>) -> Result<llm::VisionStatus> {
    llm::get_vision_status(&state.working_dir()?)
}
