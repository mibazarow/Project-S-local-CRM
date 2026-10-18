use crate::{brief, db, paths, projects, watcher};
use crate::error::{AppError, Result};
use crate::projects::ProjectDto;
use crate::state::{AppState};
use super::{with_db, emit_fs_changed, emit_projects_changed};
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub(crate) fn list_projects(state: State<AppState>) -> Result<Vec<ProjectDto>> {
    // Rows under the DB mutex; the per-project folder walk (file_count) runs
    // after the lock is released.
    let rows = with_db(&state, projects::list_project_rows)?;
    Ok(rows
        .into_iter()
        .map(|(id, row)| projects::dto_from_row(id, row))
        .collect())
}

#[tauri::command]
pub(crate) fn create_project(app: AppHandle, state: State<AppState>, name: String) -> Result<ProjectDto> {
    let wd = state.working_dir()?;
    // Folder tree first (outside the DB mutex), then the single INSERT; if
    // the DB fails, remove exactly what this call created.
    let made = projects::create_project_fs(&wd, &name)?;
    let (id, row) = match with_db(&state, |conn| projects::create_project_db(conn, &made)) {
        Ok(v) => v,
        Err(e) => {
            projects::undo_create(&made);
            return Err(e);
        }
    };
    let dto = projects::dto_from_row(id, row);
    watcher::start(&app, &state, &wd);
    emit_fs_changed(&app, dto.id, "");
    emit_projects_changed(&app);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn rename_project(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    new_name: String,
) -> Result<ProjectDto> {
    let wd = state.working_dir()?;
    // Resolve under a short DB read, rename on disk outside the mutex, then
    // the single UPDATE; on DB failure rename back (same compensation).
    let plan = with_db(&state, |conn| projects::rename_plan(conn, &wd, id, &new_name))?;
    projects::rename_move(&plan)?;
    let row = match with_db(&state, |conn| projects::rename_apply(conn, id, &plan)) {
        Ok(row) => row,
        Err(e) => {
            projects::rename_rollback(&plan);
            return Err(e);
        }
    };
    let dto = projects::dto_from_row(id, row);
    watcher::start(&app, &state, &wd);
    emit_fs_changed(&app, dto.id, "");
    emit_projects_changed(&app);
    Ok(dto)
}

#[tauri::command]
pub(crate) fn project_path(state: State<AppState>, id: i64) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| projects::project_path(conn, &wd, id))
}

#[tauri::command]
pub(crate) fn touch_project_opened(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| projects::touch_project_opened(conn, id))?;
    // Background brief maintenance: regenerates only when the cached brief
    // is missing or stale (>24 h); a fresh brief costs one cheap SQL check.
    brief::schedule_refresh(&app, id);
    Ok(())
}

// Cached project brief; generates it now (LLM with heuristic fallback) when
// it is missing or stale. Emits "project-brief-changed" when regenerated.
#[tauri::command]
pub(crate) async fn project_brief(
    app: AppHandle,
    state: State<'_, AppState>,
    project_id: i64,
) -> Result<brief::BriefDto> {
    let wd = state.working_dir()?;
    let (dto, generated) = tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&paths::db_path(&wd))?;
        projects::get_project(&conn, project_id)?;
        brief::ensure_brief_on(&conn, &wd, project_id, brief::MAX_AGE)
    })
    .await
    .map_err(|e| AppError::Io(format!("brief task: {e}")))??;
    if generated {
        let _ = app.emit("project-brief-changed", brief::BriefChanged { project_id });
    }
    Ok(dto)
}

// Force-regenerates the brief now, bypassing the cache.
#[tauri::command]
pub(crate) async fn refresh_project_brief(
    app: AppHandle,
    state: State<'_, AppState>,
    project_id: i64,
) -> Result<brief::BriefDto> {
    let wd = state.working_dir()?;
    let dto = tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&paths::db_path(&wd))?;
        projects::get_project(&conn, project_id)?;
        brief::refresh_brief(&conn, &wd, project_id)
    })
    .await
    .map_err(|e| AppError::Io(format!("brief task: {e}")))??;
    let _ = app.emit("project-brief-changed", brief::BriefChanged { project_id });
    Ok(dto)
}

#[tauri::command]
pub(crate) fn move_project(app: AppHandle, state: State<AppState>, id: i64, position: f64) -> Result<()> {
    with_db(&state, |conn| projects::move_project(conn, id, position))?;
    emit_projects_changed(&app);
    Ok(())
}

// Per-project kanban column names (exactly 3, trimmed, max 40 chars each).
#[tauri::command]
pub(crate) fn set_task_columns(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    columns: Vec<String>,
) -> Result<ProjectDto> {
    let dto = with_db(&state, |conn| projects::set_task_columns(conn, project_id, columns))?;
    emit_projects_changed(&app);
    Ok(dto)
}

// Per-project file view mode ("list" | "grid"); unknown values fall back to
// the default. Emits "projects-changed" so the store picks the new mode up.
#[tauri::command]
pub(crate) fn project_set_files_view(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    view: String,
) -> Result<ProjectDto> {
    let dto = with_db(&state, |conn| projects::set_files_view(conn, project_id, &view))?;
    emit_projects_changed(&app);
    Ok(dto)
}
