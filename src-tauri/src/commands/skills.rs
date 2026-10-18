use crate::{skills};
use crate::error::{AppError, Result};
use crate::state::{AppState};
use super::{emit_skills_changed};
use tauri::{AppHandle, State};

// Skills management (SPEC 6.2); the skills UI screen is the next slice.
#[tauri::command]
pub(crate) fn skills_list(state: State<AppState>) -> Result<Vec<skills::SkillInfo>> {
    Ok(skills::list(&state.working_dir()?))
}

#[tauri::command]
pub(crate) fn skills_set_enabled(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    enabled: bool,
) -> Result<()> {
    skills::set_enabled(&state.working_dir()?, &name, enabled)?;
    emit_skills_changed(&app);
    Ok(())
}

// Blocking zip extraction / git clone / file copies run on the blocking pool.
#[tauri::command]
pub(crate) async fn skills_import(
    app: AppHandle,
    state: State<'_, AppState>,
    source: String,
    target: String,
    overwrite: Option<bool>,
) -> Result<Vec<skills::ImportResult>> {
    let wd = state.working_dir()?;
    let results = tauri::async_runtime::spawn_blocking(move || {
        skills::import(&wd, &source, &target, overwrite.unwrap_or(false))
    })
    .await
    .map_err(|e| AppError::Io(format!("skills import task: {e}")))??;
    emit_skills_changed(&app);
    Ok(results)
}

#[tauri::command]
pub(crate) async fn skills_install_builtin(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<String>> {
    let wd = state.working_dir()?;
    let names = tauri::async_runtime::spawn_blocking(move || skills::install_builtin(&wd))
        .await
        .map_err(|e| AppError::Io(format!("skills builtin install task: {e}")))??;
    emit_skills_changed(&app);
    Ok(names)
}

#[tauri::command]
pub(crate) fn skills_create(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    description: String,
    kind: Option<String>,
    body: Option<String>,
    overwrite: Option<bool>,
) -> Result<skills::SkillInfo> {
    let info = skills::create(
        &state.working_dir()?,
        &name,
        &description,
        kind.as_deref(),
        body.as_deref(),
        overwrite.unwrap_or(false),
    )?;
    emit_skills_changed(&app);
    Ok(info)
}
