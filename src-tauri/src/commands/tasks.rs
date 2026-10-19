use crate::{export, tasks, todos, undo};
use crate::error::{Result};
use crate::tasks::TaskDto;
use crate::state::{AppState};
use super::{with_db, emit_tasks_changed, emit_trash_changed};
use tauri::{AppHandle, State};

#[tauri::command]
pub(crate) fn tasks_list(state: State<AppState>, project_id: i64) -> Result<Vec<TaskDto>> {
    with_db(&state, |conn| tasks::tasks_list(conn, project_id))
}

// Read-only plan panel data: scopeKind is "project" (scopeId = project id)
// or "session" (scopeId = harness session id), mirroring the tool scopes.
#[tauri::command]
pub(crate) fn todos_list(
    state: State<AppState>,
    scope_kind: String,
    scope_id: i64,
) -> Result<Vec<todos::TodoDto>> {
    with_db(&state, |conn| todos::todos_list(conn, &scope_kind, scope_id))
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) fn task_create(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    title: String,
    status: Option<String>,
    due_date: Option<String>,
    due_time: Option<String>,
    is_priority: Option<bool>,
    checklist: Option<Vec<tasks::ChecklistItem>>,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let dto = tasks::task_create_with(
            conn,
            project_id,
            &title,
            status,
            due_date,
            due_time,
            is_priority,
            checklist,
        )?;
        let _ = undo::push_task_create(conn, dto.id, dto.project_id);
        export::export_project(conn, &wd, project_id)?;
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn task_update(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    patch: tasks::TaskPatch,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let snapshot = tasks::task_snapshot(conn, id).ok();
        let dto = tasks::task_update(conn, id, patch)?;
        if let Some(snapshot) = snapshot {
            let _ = undo::push_task_snapshot(conn, "task_update", &snapshot, None);
        }
        let _ = export::export_project(conn, &wd, dto.project_id);
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn task_move(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    status: String,
    position: f64,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let snapshot = tasks::task_snapshot(conn, id).ok();
        let (dto, spawned) = tasks::task_move_capture(conn, id, &status, position)?;
        if let Some(snapshot) = snapshot {
            let _ = undo::push_task_snapshot(conn, "task_move", &snapshot, spawned);
        }
        export::export_project(conn, &wd, dto.project_id)?;
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn task_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    let wd = state.working_dir()?;
    let project_id = with_db(&state, |conn| {
        let (project_id, trash_id) = tasks::task_delete_with_trash(conn, id)?;
        let _ = undo::push_task_delete(conn, trash_id, project_id);
        export::export_project(conn, &wd, project_id)?;
        Ok(project_id)
    })?;
    emit_tasks_changed(&app, project_id);
    emit_trash_changed(&app, None);
    Ok(())
}
