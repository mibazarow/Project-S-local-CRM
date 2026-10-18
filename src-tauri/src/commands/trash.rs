use crate::{export, reliability};
use crate::error::{Result};
use crate::state::{AppState};
use super::{with_db, emit_tasks_changed, emit_trash_changed};
use tauri::{AppHandle, State};

// Trash (SPEC trash/versions slice): every delete from the UI or the agents
// lands in Settings/.trash; the panel lists, restores or purges entries.
#[tauri::command]
pub(crate) fn trash_list(state: State<AppState>) -> Result<Vec<reliability::TrashDto>> {
    with_db(&state, reliability::trash_list)
}

#[tauri::command]
pub(crate) fn trash_restore(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
) -> Result<reliability::TrashDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| reliability::trash_restore(conn, &wd, id))?;
    emit_trash_changed(&app, Some(id));
    if let Some(pid) = dto.project_id {
        // A restored task: re-export the kanban files and notify the board.
        let _ = with_db(&state, |conn| export::export_project(conn, &wd, pid));
        emit_tasks_changed(&app, pid);
    }
    Ok(dto)
}

#[tauri::command]
pub(crate) fn trash_purge(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| reliability::trash_purge(conn, &wd, id))?;
    emit_trash_changed(&app, Some(id));
    Ok(())
}

// Version history for one file: snapshots are keyed by (scope, absolute
// path); restore returns the stored content, writing it back is the caller's
// job so the normal write path (and its own snapshot) applies.
#[tauri::command]
pub(crate) fn version_list(
    state: State<AppState>,
    scope: String,
    path: String,
) -> Result<Vec<reliability::VersionDto>> {
    with_db(&state, |conn| reliability::version_list(conn, &scope, &path))
}

#[tauri::command]
pub(crate) fn version_restore(
    state: State<AppState>,
    id: i64,
) -> Result<reliability::VersionRestoreDto> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| reliability::version_restore(conn, &wd, id))
}
