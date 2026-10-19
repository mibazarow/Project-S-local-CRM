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

// Provider profiles (Q-24): list/add/update/delete/activate. Each mutation
// returns the fresh provider list so the UI refreshes without a second read.
#[tauri::command]
pub(crate) fn llm_providers_list(state: State<AppState>) -> Result<Vec<llm::LlmProviderDto>> {
    llm::providers_list(&state.working_dir()?)
}

#[tauri::command]
pub(crate) fn llm_provider_add(
    state: State<AppState>,
    name: String,
    base_url: String,
    api_key: String,
) -> Result<Vec<llm::LlmProviderDto>> {
    llm::provider_add(&state.working_dir()?, &name, &base_url, &api_key)
}

#[tauri::command]
pub(crate) fn llm_provider_update(
    state: State<AppState>,
    id: String,
    name: String,
    base_url: String,
    api_key: String,
) -> Result<Vec<llm::LlmProviderDto>> {
    llm::provider_update(&state.working_dir()?, &id, &name, &base_url, &api_key)
}

#[tauri::command]
pub(crate) fn llm_provider_delete(state: State<AppState>, id: String) -> Result<Vec<llm::LlmProviderDto>> {
    llm::provider_delete(&state.working_dir()?, &id)
}

#[tauri::command]
pub(crate) fn llm_provider_activate(state: State<AppState>, id: String) -> Result<Vec<llm::LlmProviderDto>> {
    llm::provider_activate(&state.working_dir()?, &id)
}
