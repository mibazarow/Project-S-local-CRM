use crate::{index, mcp, onboarding, processes, reliability, scheduler, search_reindex, watcher};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use crate::sync_util::lock_recover;
use super::{app_config_dir, with_db, spawn_auto_backup, open_journal};
use std::path::PathBuf;
use tauri::{AppHandle, State};

#[tauri::command]
pub(crate) fn onboarding_status(app: AppHandle) -> Result<onboarding::OnboardingStatus> {
    Ok(onboarding::status(&app_config_dir(&app)?))
}

#[tauri::command]
pub(crate) fn default_working_dir() -> String {
    onboarding::default_working_dir().display().to_string()
}

#[tauri::command]
pub(crate) fn choose_working_dir(
    app: AppHandle,
    state: State<AppState>,
    path: String,
) -> Result<onboarding::WorkingDirInfo> {
    if path.trim().is_empty() {
        return Err(AppError::Invalid("working dir path is empty".into()));
    }
    let wd = PathBuf::from(path);
    let (info, conn) = onboarding::init_working_dir(&app_config_dir(&app)?, &wd)?;
    *lock_recover(&state.db) = Some(conn);
    *lock_recover(&state.working_dir) = Some(wd.clone());
    open_journal(&state, &wd)?;
    let _ = with_db(&state, processes::reconcile);
    let _ = with_db(&state, |conn| {
        reliability::startup_purge(conn, &wd);
        Ok(())
    });
    watcher::start(&app, &state, &wd);
    let _ = index::index_notes(&wd);
    *lock_recover(&state.mcp) = mcp::McpRegistry::start(&wd);
    scheduler::start(app.clone());
    search_reindex::start_timer(app.clone());
    spawn_auto_backup(&wd);
    Ok(info)
}

#[tauri::command]
pub(crate) fn get_working_dir(state: State<AppState>) -> Option<String> {
    lock_recover(&state.working_dir)
        .as_ref()
        .map(|p| p.display().to_string())
}
