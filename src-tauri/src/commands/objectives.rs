use crate::{objectives};
use crate::error::{Result};
use crate::state::{AppState};
use super::{with_db, emit_objectives_changed};
use tauri::{AppHandle, State};

// Long-lived goals with steps (objectives slice): the Today panel lists all
// objectives; every mutation emits "objectives-changed" for open views.
#[tauri::command]
pub(crate) fn objective_create(
    app: AppHandle,
    state: State<AppState>,
    title: String,
    description: Option<String>,
    steps: Option<Vec<String>>,
) -> Result<objectives::ObjectiveDto> {
    let dto = with_db(&state, |conn| {
        objectives::objective_create(conn, &title, description, &steps.unwrap_or_default())
    })?;
    emit_objectives_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
pub(crate) fn objective_list(state: State<AppState>) -> Result<Vec<objectives::ObjectiveDto>> {
    with_db(&state, objectives::objective_list)
}

#[tauri::command]
pub(crate) fn objective_update(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    patch: objectives::ObjectivePatch,
) -> Result<objectives::ObjectiveDto> {
    let dto = with_db(&state, |conn| objectives::objective_update(conn, id, patch))?;
    emit_objectives_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
pub(crate) fn objective_step_done(
    app: AppHandle,
    state: State<AppState>,
    objective_id: i64,
    step_id: i64,
    done: bool,
) -> Result<objectives::ObjectiveDto> {
    let dto = with_db(&state, |conn| {
        objectives::objective_step_done(conn, objective_id, step_id, done)
    })?;
    emit_objectives_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
pub(crate) fn objective_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| objectives::objective_delete(conn, id))?;
    emit_objectives_changed(&app, Some(id));
    Ok(())
}
