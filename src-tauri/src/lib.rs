mod agent;
mod attachments;
mod brief;
mod cdp;
mod chat;
mod config;
mod db;
mod embed;
mod error;
mod extract;
mod export;
mod fs_ops;
mod harness;
mod index;
mod importer;
mod journal;
mod links;
mod llm;
mod mcp;
mod memory;
mod notes;
mod objectives;
mod onboarding;
mod open_with;
mod ops;
mod path_policy;
mod paths;
mod processes;
mod projects;
mod reliability;
mod scheduler;
mod search;
mod search_reindex;
mod skills;
mod state;
pub mod sync_util;
mod tasks;
mod templates;
mod todos;
mod undo;
mod watcher;

use agent::permissions::ConfirmDecision;
use error::{AppError, Result};
use fs_ops::FsEntry;
use projects::ProjectDto;
use tasks::TaskDto;
use rusqlite::Connection;
use serde_json::Value;
use state::{AgentRegistry, AppState, RunKey};
use sync_util::lock_recover;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

fn app_config_dir(app: &AppHandle) -> Result<PathBuf> {
    app.path()
        .app_config_dir()
        .map_err(|e| AppError::Io(e.to_string()))
}

fn with_db<T>(state: &AppState, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let guard = lock_recover(&state.db);
    f(guard.as_ref().ok_or(AppError::NotConfigured)?)
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FsChanged {
    project_id: i64,
    rel_path: String,
}

fn emit_fs_changed(app: &AppHandle, project_id: i64, rel_path: impl Into<String>) {
    let _ = app.emit(
        "fs-changed",
        FsChanged {
            project_id,
            rel_path: rel_path.into(),
        },
    );
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TasksChanged {
    project_id: i64,
}

fn emit_tasks_changed(app: &AppHandle, project_id: i64) {
    let _ = app.emit("tasks-changed", TasksChanged { project_id });
}

// Objectives (Today panel goals with steps): every mutation reloads the list.
fn emit_objectives_changed(app: &AppHandle, id: Option<i64>) {
    let _ = app.emit(
        "objectives-changed",
        objectives::ObjectivesChanged { id },
    );
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryChanged {
    id: Option<i64>,
}

fn emit_memory_changed(app: &AppHandle, id: Option<i64>) {
    let _ = app.emit("memory-changed", MemoryChanged { id });
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrashChanged {
    id: Option<i64>,
}

fn emit_trash_changed(app: &AppHandle, id: Option<i64>) {
    let _ = app.emit("trash-changed", TrashChanged { id });
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionsChanged {
    scope: String,
    path: String,
}

fn emit_versions_changed(app: &AppHandle, scope: &str, path: &str) {
    let _ = app.emit(
        "versions-changed",
        VersionsChanged {
            scope: scope.into(),
            path: path.into(),
        },
    );
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SchedulesChanged {}

fn emit_schedules_changed(app: &AppHandle) {
    let _ = app.emit("schedules-changed", SchedulesChanged {});
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillsChanged {}

fn emit_skills_changed(app: &AppHandle) {
    let _ = app.emit("skills-changed", SkillsChanged {});
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectsChanged {}

fn emit_projects_changed(app: &AppHandle) {
    let _ = app.emit("projects-changed", ProjectsChanged {});
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NotesChanged {
    rel_path: String,
}

fn emit_notes_changed(app: &AppHandle, rel_path: impl Into<String>) {
    let _ = app.emit(
        "notes-changed",
        NotesChanged {
            rel_path: rel_path.into(),
        },
    );
}

#[tauri::command]
fn ping() -> String {
    "pong".into()
}

#[tauri::command]
fn onboarding_status(app: AppHandle) -> Result<onboarding::OnboardingStatus> {
    Ok(onboarding::status(&app_config_dir(&app)?))
}

#[tauri::command]
fn default_working_dir() -> String {
    onboarding::default_working_dir().display().to_string()
}

#[tauri::command]
fn choose_working_dir(
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
fn get_working_dir(state: State<AppState>) -> Option<String> {
    lock_recover(&state.working_dir)
        .as_ref()
        .map(|p| p.display().to_string())
}

#[tauri::command]
fn config_dir_path(state: State<AppState>) -> Result<String> {
    Ok(paths::config_dir(&state.working_dir()?).display().to_string())
}

#[tauri::command]
fn read_config(state: State<AppState>, kind: config::ConfigKind) -> Result<Value> {
    config::read_config(&state.working_dir()?, kind)
}

#[tauri::command]
fn write_config(state: State<AppState>, kind: config::ConfigKind, value: Value) -> Result<()> {
    config::write_config(&state.working_dir()?, kind, value)
}

#[tauri::command]
fn list_projects(state: State<AppState>) -> Result<Vec<ProjectDto>> {
    // Rows under the DB mutex; the per-project folder walk (file_count) runs
    // after the lock is released.
    let rows = with_db(&state, projects::list_project_rows)?;
    Ok(rows
        .into_iter()
        .map(|(id, row)| projects::dto_from_row(id, row))
        .collect())
}

#[tauri::command]
fn create_project(app: AppHandle, state: State<AppState>, name: String) -> Result<ProjectDto> {
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
fn rename_project(
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
fn project_path(state: State<AppState>, id: i64) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| projects::project_path(conn, &wd, id))
}

#[tauri::command]
fn touch_project_opened(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| projects::touch_project_opened(conn, id))?;
    // Background brief maintenance: regenerates only when the cached brief
    // is missing or stale (>24 h); a fresh brief costs one cheap SQL check.
    brief::schedule_refresh(&app, id);
    Ok(())
}

// Cached project brief; generates it now (LLM with heuristic fallback) when
// it is missing or stale. Emits "project-brief-changed" when regenerated.
#[tauri::command]
async fn project_brief(
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
async fn refresh_project_brief(
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
fn move_project(app: AppHandle, state: State<AppState>, id: i64, position: f64) -> Result<()> {
    with_db(&state, |conn| projects::move_project(conn, id, position))?;
    emit_projects_changed(&app);
    Ok(())
}

// Per-project kanban column names (exactly 3, trimmed, max 40 chars each).
#[tauri::command]
fn set_task_columns(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    columns: Vec<String>,
) -> Result<ProjectDto> {
    let dto = with_db(&state, |conn| projects::set_task_columns(conn, project_id, columns))?;
    emit_projects_changed(&app);
    Ok(dto)
}

#[tauri::command]
fn tasks_list(state: State<AppState>, project_id: i64) -> Result<Vec<TaskDto>> {
    with_db(&state, |conn| tasks::tasks_list(conn, project_id))
}

// Read-only plan panel data: scopeKind is "project" (scopeId = project id)
// or "session" (scopeId = harness session id), mirroring the tool scopes.
#[tauri::command]
fn todos_list(
    state: State<AppState>,
    scope_kind: String,
    scope_id: i64,
) -> Result<Vec<todos::TodoDto>> {
    with_db(&state, |conn| todos::todos_list(conn, &scope_kind, scope_id))
}

// Long-lived goals with steps (objectives slice): the Today panel lists all
// objectives; every mutation emits "objectives-changed" for open views.
#[tauri::command]
fn objective_create(
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
fn objective_list(state: State<AppState>) -> Result<Vec<objectives::ObjectiveDto>> {
    with_db(&state, objectives::objective_list)
}

#[tauri::command]
fn objective_update(
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
fn objective_step_done(
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
fn objective_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| objectives::objective_delete(conn, id))?;
    emit_objectives_changed(&app, Some(id));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
fn task_create(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    title: String,
    status: Option<String>,
    due_date: Option<String>,
    is_priority: Option<bool>,
    checklist: Option<Vec<tasks::ChecklistItem>>,
    recurrence: Option<String>,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let dto = tasks::task_create_with(
            conn,
            project_id,
            &title,
            status,
            due_date,
            is_priority,
            checklist,
            recurrence,
        )?;
        let _ = undo::push_task_create(conn, dto.id, dto.project_id);
        export::export_project(conn, &wd, project_id)?;
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
fn task_update(
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
        export::export_project(conn, &wd, dto.project_id)?;
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
fn task_move(
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
fn task_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
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

// Entity links (links slice): the frontend records context-action links
// (note ↔ file/chat, task ↔ chat) after creating an entity from a file or a
// message; the agent loop records tool-call links on its own. Deduped by the
// UNIQUE constraint, so a repeated call is a no-op.
#[tauri::command]
fn link_create(
    state: State<AppState>,
    from_kind: String,
    from_id: String,
    to_kind: String,
    to_id: String,
) -> Result<()> {
    with_db(&state, |conn| {
        links::create(conn, &from_kind, &from_id, &to_kind, &to_id)
    })
}

#[tauri::command]
fn fs_list_dir(
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<Vec<FsEntry>> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::list_dir(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
fn fs_read_text(state: State<AppState>, project_id: i64, rel_path: String) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::read_text(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
fn fs_write_text(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
    content: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let previous = with_db(&state, |conn| {
        let path = fs_ops::resolve_existing(conn, &wd, project_id, &rel_path)?;
        let abs = path.display().to_string();
        let version_id = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| reliability::version_save(conn, &wd, "project", &abs, &c).ok());
        Ok::<Option<(String, i64)>, AppError>(version_id.map(|id| (abs, id)))
    })
    .unwrap_or(None);
    let item = with_db(&state, |conn| {
        fs_ops::write_text(conn, &wd, project_id, &rel_path, &content)
    })?;
    if let Some((abs, version_id)) = previous {
        let _ = with_db(&state, |conn| {
            undo::push_file_write(
                conn,
                "project",
                &abs,
                Some(version_id),
                false,
                Some(project_id),
                Some(&item.rel_path),
            )
        });
    }
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_create_folder(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    parent_rel: String,
    name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| {
        fs_ops::create_folder(conn, &wd, project_id, &parent_rel, &name)
    })?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_create_file(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    parent_rel: String,
    name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| {
        fs_ops::create_file(conn, &wd, project_id, &parent_rel, &name)
    })?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_rename(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
    new_name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = with_db(&state, |conn| {
        fs_ops::resolve_existing(conn, &wd, project_id, &rel_path)
            .map(|p| p.display().to_string())
    })
    .ok();
    let item = with_db(&state, |conn| {
        fs_ops::rename(conn, &wd, project_id, &rel_path, &new_name)
    })?;
    if let Some(from_abs) = from_abs {
        let to_abs = with_db(&state, |conn| {
            fs_ops::resolve_existing(conn, &wd, project_id, &item.rel_path)
                .map(|p| p.display().to_string())
        })
        .ok();
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_rename(
                    conn,
                    "project",
                    &from_abs,
                    &to_abs,
                    Some(project_id),
                    Some(&item.rel_path),
                )
            });
        }
    }
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_copy_as(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| fs_ops::copy_as(conn, &wd, project_id, &rel_path))?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_absolute_path(state: State<AppState>, project_id: i64, rel_path: String) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::absolute_path(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
fn fs_reveal_in_finder(state: State<AppState>, project_id: i64, rel_path: String) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| fs_ops::reveal_in_finder(conn, &wd, project_id, &rel_path))
}

#[tauri::command]
fn fs_extract_archive(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| fs_ops::extract_archive(conn, &wd, project_id, &rel_path))?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_delete(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<()> {
    let wd = state.working_dir()?;
    let path_abs = with_db(&state, |conn| {
        fs_ops::resolve_existing(conn, &wd, project_id, &rel_path)
            .map(|p| p.display().to_string())
    })
    .ok();
    // Resolve under a short DB read, move the item into the trash store
    // outside the DB mutex (a folder move can copy a whole tree), then the
    // single trash-row INSERT. Same order as before: move, then row.
    let (kind, path) = with_db(&state, |conn| {
        fs_ops::delete_plan(conn, &wd, project_id, &rel_path)
    })?;
    let staged = reliability::trash_stage(&wd, &path)?;
    let trash_id = with_db(&state, |conn| {
        reliability::trash_record(conn, "project", &kind, &path, &staged, None)
    })?;
    if let Some(path_abs) = path_abs {
        let _ = with_db(&state, |conn| {
            undo::push_delete(
                conn,
                trash_id,
                "project",
                &path_abs,
                Some(project_id),
                Some(&rel_path),
            )
        });
    }
    emit_fs_changed(&app, project_id, rel_path);
    emit_trash_changed(&app, Some(trash_id));
    Ok(())
}

#[tauri::command]
fn fs_move(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    from_rel: String,
    to_dir_rel: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = with_db(&state, |conn| {
        fs_ops::resolve_existing(conn, &wd, project_id, &from_rel)
            .map(|p| p.display().to_string())
    })
    .ok();
    let item = with_db(&state, |conn| {
        fs_ops::move_entry(conn, &wd, project_id, &from_rel, &to_dir_rel)
    })?;
    if let Some(from_abs) = from_abs {
        let to_abs = with_db(&state, |conn| {
            fs_ops::resolve_existing(conn, &wd, project_id, &item.rel_path)
                .map(|p| p.display().to_string())
        })
        .ok();
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_rename(
                    conn,
                    "project",
                    &from_abs,
                    &to_abs,
                    Some(project_id),
                    Some(&item.rel_path),
                )
            });
        }
    }
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_import(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    dest_rel: String,
    source_paths: Vec<String>,
) -> Result<Vec<FsEntry>> {
    let wd = state.working_dir()?;
    let items = with_db(&state, |conn| {
        fs_ops::import(conn, &wd, project_id, &dest_rel, source_paths)
    })?;
    emit_fs_changed(&app, project_id, dest_rel);
    Ok(items)
}

#[tauri::command]
fn fs_open_external(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        fs_ops::open_external(&app, conn, &wd, project_id, &rel_path)
    })
}

// "Open with" chooser: the LaunchServices handler apps for an extension
// (non-stock first), the remembered per-extension choice, and opening a file
// with a chosen app via `open -a`.
#[tauri::command]
fn list_open_with_apps(ext: String) -> Vec<open_with::AppInfo> {
    open_with::list_apps(&ext)
}

#[tauri::command]
fn get_open_with(state: State<AppState>, ext: String) -> Result<Option<open_with::AppInfo>> {
    open_with::get_remembered(&state.working_dir()?, &ext)
}

#[tauri::command]
fn remember_open_with(state: State<AppState>, ext: String, app_path: String) -> Result<()> {
    open_with::remember(&state.working_dir()?, &ext, &app_path)
}

#[tauri::command]
fn open_with(
    state: State<AppState>,
    scope: String,
    project_id: Option<i64>,
    path: String,
    app_path: Option<String>,
) -> Result<open_with::OpenResult> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        open_with::open_with(conn, &wd, &scope, project_id, &path, app_path.as_deref())
    })
}

#[tauri::command]
fn fs_read_binary_data_url(
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
    max_bytes: u64,
) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        fs_ops::read_binary_data_url(conn, &wd, project_id, &rel_path, max_bytes)
    })
}

// Notes vault (top-level Notes/ folder): browse, edit and search markdown
// notes. Paths are vault-relative; the watcher reindexes Notes/ debounced,
// mutations notify the UI with "notes-changed" (and "index-changed" with
// project_id = NOTES_SCOPE_ID once the reindex lands).
#[tauri::command]
fn notes_ensure(state: State<AppState>) -> Result<String> {
    let dir = notes::ensure(&state.working_dir()?)?;
    Ok(dir.display().to_string())
}

#[tauri::command]
fn notes_tree(state: State<AppState>) -> Result<Vec<FsEntry>> {
    notes::tree(&state.working_dir()?)
}

#[tauri::command]
fn notes_list(state: State<AppState>, rel: Option<String>) -> Result<Vec<FsEntry>> {
    notes::list(&state.working_dir()?, rel.as_deref().unwrap_or(""))
}

#[tauri::command]
fn notes_read(state: State<AppState>, rel: String) -> Result<String> {
    notes::read(&state.working_dir()?, &rel)
}

#[tauri::command]
fn notes_write(
    app: AppHandle,
    state: State<AppState>,
    rel: String,
    content: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    // Snapshot the previous content before an overwrite (version history);
    // the absolute vault path is the same key the notes_write tool uses.
    let previous = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &rel, "Notes").ok())
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|c| (p.display().to_string(), c)));
    let item = notes::write(&wd, &rel, &content)?;
    let version_id = previous
        .as_ref()
        .and_then(|(abs, prev)| {
            let id = with_db(&state, |conn| {
                reliability::version_save(conn, &wd, "notes", abs, prev)
            })
            .ok();
            if id.is_some() {
                emit_versions_changed(&app, "notes", abs);
            }
            id
        });
    if let (Some((abs, _)), Some(version_id)) = (&previous, version_id) {
        let _ = with_db(&state, |conn| {
            undo::push_file_write(
                conn,
                "notes",
                abs,
                Some(version_id),
                false,
                None,
                Some(&item.rel_path),
            )
        });
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn notes_create(
    app: AppHandle,
    state: State<AppState>,
    rel: String,
    content: Option<String>,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = notes::create(&wd, &rel, content.as_deref())?;
    let created_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
        .map(|p| p.display().to_string());
    if let Some(created_abs) = created_abs {
        let _ = with_db(&state, |conn| {
            undo::push_file_write(
                conn,
                "notes",
                &created_abs,
                None,
                true,
                None,
                Some(&item.rel_path),
            )
        });
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn notes_rename(
    app: AppHandle,
    state: State<AppState>,
    rel: String,
    new_name: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &rel, "Notes").ok())
        .map(|p| p.display().to_string());
    let item = notes::rename(&wd, &rel, &new_name)?;
    if let Some(from_abs) = from_abs {
        let to_abs = notes::vault_root(&wd)
            .ok()
            .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
            .map(|p| p.display().to_string());
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_rename(conn, "notes", &from_abs, &to_abs, None, Some(&item.rel_path))
            });
        }
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn notes_mkdir(
    app: AppHandle,
    state: State<AppState>,
    parent_rel: Option<String>,
    name: String,
) -> Result<FsEntry> {
    let item = notes::mkdir(
        &state.working_dir()?,
        parent_rel.as_deref().unwrap_or(""),
        &name,
    )?;
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

// Move never overwrites: a name clash in the target folder is an error.
#[tauri::command]
fn notes_move(
    app: AppHandle,
    state: State<AppState>,
    from_rel: String,
    to_dir_rel: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let from_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &from_rel, "Notes").ok())
        .map(|p| p.display().to_string());
    let item = notes::move_entry(&wd, &from_rel, &to_dir_rel)?;
    if let Some(from_abs) = from_abs {
        let to_abs = notes::vault_root(&wd)
            .ok()
            .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
            .map(|p| p.display().to_string());
        if let Some(to_abs) = to_abs {
            let _ = with_db(&state, |conn| {
                undo::push_note_move(conn, &from_abs, &to_abs, &item.rel_path)
            });
        }
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

// Copy keeps the source and suffixes the new entry on a name clash.
#[tauri::command]
fn notes_copy(
    app: AppHandle,
    state: State<AppState>,
    from_rel: String,
    to_dir_rel: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = notes::copy(&wd, &from_rel, &to_dir_rel)?;
    let created_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &item.rel_path, "Notes").ok())
        .map(|p| p.display().to_string());
    if let Some(created_abs) = created_abs {
        let _ = with_db(&state, |conn| {
            undo::push_note_copy(conn, &created_abs, &item.rel_path)
        });
    }
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn notes_delete(app: AppHandle, state: State<AppState>, rel: String) -> Result<()> {
    let wd = state.working_dir()?;
    let path_abs = notes::vault_root(&wd)
        .ok()
        .and_then(|root| fs_ops::contain_existing(&root, &rel, "Notes").ok())
        .map(|p| p.display().to_string());
    let trash_id = with_db(&state, |conn| notes::delete(conn, &wd, &rel))?;
    if let Some(path_abs) = path_abs {
        let _ = with_db(&state, |conn| {
            undo::push_delete(conn, trash_id, "notes", &path_abs, None, Some(&rel))
        });
    }
    emit_notes_changed(&app, rel);
    emit_trash_changed(&app, Some(trash_id));
    Ok(())
}

// Bounded undo stack (SPEC undo slice): undo_last pops and reverses the newest
// mutation, undo_list previews the stack. Undo is best-effort: a failed undo is
// reported to the caller, but the mutation that pushed the entry already ran.
#[tauri::command]
fn undo_last(app: AppHandle, state: State<AppState>) -> Result<undo::UndoOutcome> {
    let wd = state.working_dir()?;
    let out = with_db(&state, |conn| undo::undo_last(conn, &wd))?;
    if let Some(fs) = &out.fs_event {
        emit_fs_changed(&app, fs.project_id, fs.rel_path.clone());
    }
    if let Some(rel) = &out.notes_rel_path {
        emit_notes_changed(&app, rel.clone());
    }
    if let Some(project_id) = out.task_project_id {
        emit_tasks_changed(&app, project_id);
    }
    if out.trash_id.is_some() {
        emit_trash_changed(&app, out.trash_id);
    }
    Ok(out)
}

#[tauri::command]
fn undo_list(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<undo::UndoEntryDto>> {
    with_db(&state, |conn| undo::list(conn, limit))
}

// Quick capture (SPEC quick capture slice): the capture modal (global
// shortcut / tray menu) persists one entry either as a timestamped bullet in
// the daily Notes/Входящие note or as a new task in the chosen project
// (falling back to the first project when none is selected).
#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum CaptureTarget {
    Note,
    Task,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct QuickCaptureResult {
    kind: &'static str,
    path: Option<String>,
    task_id: Option<i64>,
}

#[tauri::command]
fn quick_capture(
    app: AppHandle,
    state: State<AppState>,
    text: String,
    target: CaptureTarget,
    project_id: Option<i64>,
) -> Result<QuickCaptureResult> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("capture text is empty".into()));
    }
    let wd = state.working_dir()?;
    match target {
        CaptureTarget::Note => {
            let rel = notes::append_inbox(&wd, text)?;
            // Best-effort: make the new bullet searchable right away.
            let _ = index::index_notes(&wd);
            emit_notes_changed(&app, rel.clone());
            Ok(QuickCaptureResult {
                kind: "note",
                path: Some(rel),
                task_id: None,
            })
        }
        CaptureTarget::Task => {
            let title: String = text.chars().take(200).collect();
            let dto = with_db(&state, |conn| {
                let pid = match project_id {
                    Some(id) => id,
                    None => projects::first_project_id(conn)?.ok_or_else(|| {
                        AppError::Invalid("no projects: create a project first".into())
                    })?,
                };
                let dto = tasks::task_create_with(
                    conn,
                    pid,
                    &title,
                    Some("new".into()),
                    None,
                    None,
                    None,
                    None,
                )?;
                export::export_project(conn, &wd, pid)?;
                Ok(dto)
            })?;
            emit_tasks_changed(&app, dto.project_id);
            Ok(QuickCaptureResult {
                kind: "task",
                path: None,
                task_id: Some(dto.id),
            })
        }
    }
}

// Reusable templates (Settings/config/templates.json, seeded on first use):
// pickers list name + short description; the create commands instantiate one
// template as a project / task / note. template_document only returns the
// markdown, which the agent or UI fills in and creates via create_document.
#[tauri::command]
fn templates_list(state: State<AppState>) -> Result<templates::TemplatesSummary> {
    templates::summary(&state.working_dir()?)
}

#[tauri::command]
fn template_create_project(
    app: AppHandle,
    state: State<AppState>,
    template_name: String,
    project_name: Option<String>,
) -> Result<ProjectDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        templates::create_project(conn, &wd, &template_name, project_name.as_deref())
    })?;
    watcher::start(&app, &state, &wd);
    emit_fs_changed(&app, dto.id, "");
    emit_projects_changed(&app);
    Ok(dto)
}

#[tauri::command]
fn template_create_task(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    template_name: String,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let dto = templates::create_task(conn, &wd, project_id, &template_name)?;
        export::export_project(conn, &wd, project_id)?;
        Ok(dto)
    })?;
    emit_tasks_changed(&app, dto.project_id);
    Ok(dto)
}

#[tauri::command]
fn template_create_note(
    app: AppHandle,
    state: State<AppState>,
    template_name: String,
    note_name: Option<String>,
) -> Result<FsEntry> {
    let item = templates::create_note(
        &state.working_dir()?,
        &template_name,
        note_name.as_deref(),
    )?;
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn template_document(state: State<AppState>, template_name: String) -> Result<templates::DocumentDto> {
    templates::document(&state.working_dir()?, &template_name)
}

// Trash (SPEC trash/versions slice): every delete from the UI or the agents
// lands in Settings/.trash; the panel lists, restores or purges entries.
#[tauri::command]
fn trash_list(state: State<AppState>) -> Result<Vec<reliability::TrashDto>> {
    with_db(&state, reliability::trash_list)
}

#[tauri::command]
fn trash_restore(
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
fn trash_purge(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| reliability::trash_purge(conn, &wd, id))?;
    emit_trash_changed(&app, Some(id));
    Ok(())
}

// Version history for one file: snapshots are keyed by (scope, absolute
// path); restore returns the stored content, writing it back is the caller's
// job so the normal write path (and its own snapshot) applies.
#[tauri::command]
fn version_list(
    state: State<AppState>,
    scope: String,
    path: String,
) -> Result<Vec<reliability::VersionDto>> {
    with_db(&state, |conn| reliability::version_list(conn, &scope, &path))
}

#[tauri::command]
fn version_restore(
    state: State<AppState>,
    id: i64,
) -> Result<reliability::VersionRestoreDto> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| reliability::version_restore(conn, &wd, id))
}

// Notes chat (SPEC notes chat slice): one continuous chat per working dir.
// Get-or-creates the singleton and returns its chat id.
#[tauri::command]
fn ensure_notes_chat(state: State<AppState>) -> Result<i64> {
    with_db(&state, notes::ensure_notes_chat)
}

#[tauri::command]
fn notes_chat_history(state: State<AppState>) -> Result<Vec<chat::MessageDto>> {
    with_db(&state, notes::chat_history)
}

// Persists the user turn into the notes chat and starts the notes agent on a
// background thread; progress arrives as "agent-event" /
// "agent-confirmation" events carrying { chatId, notes: true }. Returns the
// notes chat id.
#[tauri::command]
fn send_notes_message(
    app: AppHandle,
    state: State<AppState>,
    registry: State<'_, Arc<AgentRegistry>>,
    text: String,
) -> Result<i64> {
    notes::run_notes_chat(&app, &state, registry.inner(), &text)
}

#[tauri::command]
fn cancel_notes_run(registry: State<'_, Arc<AgentRegistry>>) -> Result<bool> {
    Ok(registry.request_cancel(RunKey::Notes))
}

#[tauri::command]
fn search_notes(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::SearchHit>> {
    let limit = limit.unwrap_or(30).clamp(1, 100);
    search::search_notes(&state, &query, limit)
}

#[tauri::command]
fn index_notes(app: AppHandle, state: State<AppState>) -> Result<index::IndexStats> {
    let stats = index::index_notes(&state.working_dir()?)?;
    let _ = app.emit(
        "index-changed",
        index::IndexChanged {
            project_id: index::NOTES_SCOPE_ID,
            stats: stats.clone(),
        },
    );
    Ok(stats)
}

// Chat attachments (SPEC 6.1): copy files into the project's `Входящие/` (or
// `Harness/Входящие/` for a harness session), journal project copies in
// `INBOX.md`, and index them so they are searchable. Exactly one of projectId /
// sessionId selects the target.
#[tauri::command]
fn attach_files(
    app: AppHandle,
    state: State<AppState>,
    project_id: Option<i64>,
    session_id: Option<i64>,
    paths: Vec<String>,
) -> Result<Vec<attachments::AttachmentDto>> {
    let wd = state.working_dir()?;
    match (project_id, session_id) {
        (Some(pid), None) => {
            let items = with_db(&state, |conn| {
                attachments::attach_project(conn, &wd, pid, &paths)
            })?;
            // Best-effort: make the copies searchable now; the watcher reindexes
            // again on the fs-changed event.
            let _ = index::index_project(&state, pid);
            emit_fs_changed(&app, pid, paths::PROJECT_INBOX);
            Ok(items)
        }
        (None, Some(sid)) => with_db(&state, |conn| {
            attachments::attach_harness(conn, &wd, sid, &paths)
        }),
        _ => Err(AppError::Invalid(
            "exactly one of projectId or sessionId is required".into(),
        )),
    }
}

// Reads an attachment image as a base64 data URL for chat thumbnails.
// savedPath is project-relative for project attachments and absolute for
// harness ones (and for pending source paths before saving); the resolver in
// attachments.rs handles both.
#[tauri::command]
fn attachment_data_url(
    state: State<AppState>,
    project_id: Option<i64>,
    saved_path: String,
    max_bytes: Option<u64>,
) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::attachment_data_url(
            conn,
            &wd,
            project_id,
            &saved_path,
            max_bytes.unwrap_or(20 * 1024 * 1024),
        )
    })
}

// Reads one attachment image of a chat message as a base64 data URL, for the
// chat UI to render agent-sent files (SPEC 6.1). The savedPath is resolved by
// the chat's kind (project / notes / harness). Images only; index selects the
// Nth attachment of the message (0-based).
#[tauri::command]
fn chat_attachment_data_url(
    state: State<AppState>,
    chat_id: i64,
    message_id: i64,
    index: usize,
    max_bytes: Option<u64>,
) -> Result<String> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::chat_attachment_data_url(
            conn,
            &wd,
            chat_id,
            message_id,
            index,
            max_bytes.unwrap_or(20 * 1024 * 1024),
        )
    })
}

// Opens one attachment (document or image) of a chat message with the macOS
// default handler, for the chat UI to open agent-sent files (SPEC 6.1). The
// savedPath is resolved by the chat's kind (project / notes / harness); the
// attachment must belong to that chat/message or the call is refused.
#[tauri::command]
fn chat_attachment_open(
    app: AppHandle,
    state: State<AppState>,
    chat_id: i64,
    message_id: i64,
    index: usize,
) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::chat_attachment_open(&app, conn, &wd, chat_id, message_id, index)
    })
}

// Opens an attachment (document or image) with the macOS default handler.
#[tauri::command]
fn attachment_open(
    app: AppHandle,
    state: State<AppState>,
    project_id: Option<i64>,
    saved_path: String,
) -> Result<()> {
    let wd = state.working_dir()?;
    with_db(&state, |conn| {
        attachments::open_attachment(&app, conn, &wd, project_id, &saved_path)
    })
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectIndexStats {
    project_id: i64,
    stats: index::IndexStats,
}

#[tauri::command]
fn index_project(app: AppHandle, state: State<AppState>, project_id: i64) -> Result<index::IndexStats> {
    let stats = index::index_project(&state, project_id)?;
    let _ = app.emit(
        "index-changed",
        index::IndexChanged {
            project_id,
            stats: stats.clone(),
        },
    );
    Ok(stats)
}

#[tauri::command]
fn reindex_all(app: AppHandle, state: State<AppState>) -> Result<Vec<ProjectIndexStats>> {
    let results = index::index_all(&state)?;
    Ok(results
        .into_iter()
        .map(|(project_id, stats)| {
            let _ = app.emit(
                "index-changed",
                index::IndexChanged {
                    project_id,
                    stats: stats.clone(),
                },
            );
            ProjectIndexStats { project_id, stats }
        })
        .collect())
}

#[tauri::command]
fn index_status(state: State<AppState>, project_id: i64) -> Result<index::IndexStatus> {
    index::index_status(&state, project_id)
}

// Search reindex («Переиндексация поиска»): manual start from the top bar;
// the pass runs on its own thread and reports via "reindex-progress" /
// "reindex-done". The same pass also fires automatically every 5 minutes
// (background tick started in setup / choose_working_dir).
#[tauri::command]
fn reindex_start(app: AppHandle, state: State<AppState>) -> Result<search_reindex::ReindexStatus> {
    let wd = state.working_dir()?;
    Ok(search_reindex::global().start(wd, Arc::new(app)))
}

#[tauri::command]
fn reindex_status() -> search_reindex::ReindexStatus {
    search_reindex::global().status()
}

#[tauri::command]
fn embed_status(state: State<AppState>) -> Result<embed::EmbedStatus> {
    let wd = state.working_dir()?;
    Ok(embed::status(&wd))
}

// Downloads the EmbeddingGemma 2 text encoder from Hugging Face (needs the
// ort-backend feature for TLS + sha256 verification) and returns the new
// embed status. Progress is emitted as "embed-model-progress" events.
#[tauri::command]
async fn download_embed_model(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<embed::EmbedStatus> {
    let wd = state.working_dir()?;
    let emitter = app.clone();
    let download_dir = wd.clone();
    tauri::async_runtime::spawn_blocking(move || {
        embed::model_store::download_text_model(&download_dir, |file, done, total| {
            let _ = emitter.emit(
                "embed-model-progress",
                serde_json::json!({ "file": file, "done": done, "total": total }),
            );
        })
    })
    .await
    .map_err(|e| AppError::Io(format!("embed model download task: {e}")))??;
    Ok(embed::status(&wd))
}

#[tauri::command]
fn semantic_search(
    state: State<AppState>,
    project_id: i64,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<index::SemanticHit>> {
    let limit = limit.unwrap_or(10).clamp(1, 100);
    index::semantic_search(&state, project_id, &query, limit)
}

#[tauri::command]
fn search_project(
    state: State<AppState>,
    project_id: i64,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::SearchHit>> {
    let limit = limit.unwrap_or(30).clamp(1, 100);
    search::search_project(&state, project_id, &query, limit)
}

#[tauri::command]
fn search_chats(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::ChatHit>> {
    let limit = limit.unwrap_or(10).clamp(1, 50);
    search::search_chats(&state, &query, limit)
}

// Global search over all entities for the ⌘K palette.
#[tauri::command]
fn global_search(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<search::GlobalHit>> {
    let limit = limit.unwrap_or(30).clamp(1, 100);
    search::global_search(&state, &query, limit)
}

#[tauri::command]
fn index_chat_messages(state: State<AppState>) -> Result<usize> {
    chat::index_chat_messages(&state)
}

#[tauri::command]
async fn list_models(state: State<'_, AppState>) -> Result<Vec<llm::ModelInfo>> {
    let wd = state.working_dir()?;
    tauri::async_runtime::spawn_blocking(move || llm::client_from_config(&wd)?.list_models())
        .await
        .map_err(|e| AppError::Io(format!("llm task: {e}")))?
}

#[tauri::command]
async fn test_connection(state: State<'_, AppState>) -> Result<llm::ConnectionTest> {
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
async fn detect_vision(state: State<'_, AppState>) -> Result<llm::VisionStatus> {
    let wd = state.working_dir()?;
    tauri::async_runtime::spawn_blocking(move || llm::detect_vision(&wd))
        .await
        .map_err(|e| AppError::Io(format!("vision probe task: {e}")))?
}

// Cached/overridden vision status without any network call.
#[tauri::command]
fn get_vision_status(state: State<AppState>) -> Result<llm::VisionStatus> {
    llm::get_vision_status(&state.working_dir()?)
}

// Persists the user turn into the project's chat (with links to attachment
// copies already saved by attach_files) and starts the agent loop on a
// background thread; progress arrives as "agent-event" and
// "agent-confirmation" events. Returns the chat id.
#[tauri::command]
fn send_project_message(
    app: AppHandle,
    state: State<AppState>,
    registry: State<'_, Arc<AgentRegistry>>,
    project_id: i64,
    text: String,
    attachments: Option<Vec<attachments::AttachmentRef>>,
) -> Result<i64> {
    chat::start_project_run(
        &app,
        &state,
        registry.inner(),
        project_id,
        &text,
        attachments.as_deref().unwrap_or(&[]),
    )
}

#[tauri::command]
fn resolve_confirmation(
    registry: State<'_, Arc<AgentRegistry>>,
    id: u64,
    decision: String,
) -> Result<()> {
    let decision = match decision.as_str() {
        "allow" => ConfirmDecision::Allow,
        "deny" => ConfirmDecision::Deny,
        "allowAlways" => ConfirmDecision::AllowAlways,
        other => return Err(AppError::Invalid(format!("unknown decision: {other}"))),
    };
    registry.resolve(id, decision)
}

#[tauri::command]
fn project_chat_history(
    state: State<AppState>,
    project_id: i64,
) -> Result<Vec<chat::MessageDto>> {
    with_db(&state, |conn| chat::chat_history(conn, project_id))
}

// Sets the cancel flag of the project's active run; the loop stops between
// steps. Returns false when no run is active.
#[tauri::command]
fn cancel_run(registry: State<'_, Arc<AgentRegistry>>, project_id: i64) -> Result<bool> {
    Ok(registry.request_cancel(RunKey::Project(project_id)))
}

// Harness sessions (SPEC 5.1): independent chats sharing only global memory.
#[tauri::command]
fn harness_create_session(
    state: State<AppState>,
    title: Option<String>,
) -> Result<harness::SessionDto> {
    with_db(&state, |conn| harness::create_session(conn, title.as_deref()))
}

#[tauri::command]
fn harness_list_sessions(state: State<AppState>) -> Result<Vec<harness::SessionDto>> {
    with_db(&state, harness::list_sessions)
}

#[tauri::command]
fn harness_rename_session(
    state: State<AppState>,
    id: i64,
    title: String,
) -> Result<harness::SessionDto> {
    with_db(&state, |conn| harness::rename_session(conn, id, &title))
}

#[tauri::command]
fn harness_delete_session(state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| harness::delete_session(conn, id))
}

#[tauri::command]
fn harness_session_history(
    state: State<AppState>,
    id: i64,
) -> Result<Vec<chat::MessageDto>> {
    with_db(&state, |conn| harness::session_history(conn, id))
}

// Message-level chat UX (SPEC chat UX slice): pin, truncate (regenerate),
// branch (harness only), Markdown export and in-chat LIKE search.
#[tauri::command]
fn chat_pin(state: State<AppState>, chat_id: i64, message_id: i64, pinned: bool) -> Result<()> {
    with_db(&state, |conn| chat::set_pinned(conn, chat_id, message_id, pinned))
}

#[tauri::command]
fn chat_truncate_after(state: State<AppState>, chat_id: i64, message_id: i64) -> Result<usize> {
    with_db(&state, |conn| chat::truncate_after(conn, chat_id, message_id))
}

// Forks a harness session at a message; "sessions-changed" makes the fresh
// session appear in the sidebar right away (same as schedule_open).
#[tauri::command]
fn chat_branch(
    app: AppHandle,
    state: State<AppState>,
    chat_id: i64,
    message_id: i64,
) -> Result<chat::ChatBranchDto> {
    let dto = with_db(&state, |conn| chat::branch_chat(conn, chat_id, message_id))?;
    let _ = app.emit("sessions-changed", serde_json::json!({}));
    Ok(dto)
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatExportResult {
    markdown: String,
}

#[tauri::command]
fn chat_export(state: State<AppState>, chat_id: i64) -> Result<ChatExportResult> {
    let markdown = with_db(&state, |conn| chat::export_markdown(conn, chat_id))?;
    Ok(ChatExportResult { markdown })
}

#[tauri::command]
fn chat_search(
    state: State<AppState>,
    chat_id: i64,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<chat::ChatSearchHitDto>> {
    let limit = limit.unwrap_or(50).clamp(1, 200);
    with_db(&state, |conn| chat::search_messages(conn, chat_id, &query, limit))
}

// Persists the user turn into the session's chat (with links to attachment
// copies already saved by attach_files) and starts the shared agent loop on
// a background thread; progress arrives as "agent-event" /
// "agent-confirmation" events carrying sessionId. Returns the session chat id.
#[tauri::command]
fn send_session_message(
    app: AppHandle,
    state: State<AppState>,
    registry: State<'_, Arc<AgentRegistry>>,
    session_id: i64,
    text: String,
    attachments: Option<Vec<attachments::AttachmentRef>>,
) -> Result<i64> {
    harness::run_session(
        &app,
        &state,
        registry.inner(),
        session_id,
        &text,
        "harness",
        attachments.as_deref().unwrap_or(&[]),
    )
}

#[tauri::command]
fn cancel_session_run(registry: State<'_, Arc<AgentRegistry>>, session_id: i64) -> Result<bool> {
    Ok(registry.request_cancel(RunKey::Session(session_id)))
}

// Deletes harness sessions idle past app.json `sessionTtlDays` (0 disables).
#[tauri::command]
fn purge_sessions(state: State<AppState>) -> Result<usize> {
    let wd = state.working_dir()?;
    let ttl = session_ttl_days(&wd);
    with_db(&state, |conn| harness::purge_inactive_sessions(conn, ttl))
}

fn session_ttl_days(wd: &std::path::Path) -> u32 {
    config::read_config(wd, config::ConfigKind::App)
        .ok()
        .and_then(|v| serde_json::from_value::<config::AppConfig>(v).ok())
        .map(|c| c.session_ttl_days)
        .unwrap_or(30)
}

// Scheduler (SPEC 5.3): run_at / cron+IANA tz / notify over the schedules
// table; the background thread ticks every 30 s and catches up missed runs
// on the first tick after start. The UI list screen is a later slice.
#[tauri::command]
fn schedule_list(state: State<AppState>) -> Result<Vec<scheduler::ScheduleDto>> {
    with_db(&state, scheduler::list)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn schedule_create(
    app: AppHandle,
    state: State<AppState>,
    kind: String,
    spec: String,
    prompt: Option<String>,
    timezone: Option<String>,
    session_id: Option<i64>,
    title: Option<String>,
) -> Result<scheduler::ScheduleDto> {
    let dto = with_db(&state, |conn| {
        scheduler::create(
            conn,
            &scheduler::CreateIn {
                kind: &kind,
                spec: &spec,
                prompt: prompt.as_deref(),
                timezone: timezone.as_deref(),
                session_id,
                related_task_id: None,
                title: title.as_deref(),
            },
            scheduler::now_utc(),
        )
    })?;
    emit_schedules_changed(&app);
    Ok(dto)
}

#[tauri::command]
fn schedule_pause(app: AppHandle, state: State<AppState>, id: i64) -> Result<scheduler::ScheduleDto> {
    let dto = with_db(&state, |conn| scheduler::pause(conn, id))?;
    emit_schedules_changed(&app);
    Ok(dto)
}

#[tauri::command]
fn schedule_resume(app: AppHandle, state: State<AppState>, id: i64) -> Result<scheduler::ScheduleDto> {
    let dto = with_db(&state, |conn| scheduler::resume(conn, id, scheduler::now_utc()))?;
    emit_schedules_changed(&app);
    Ok(dto)
}

#[tauri::command]
fn schedule_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| scheduler::delete(conn, id))?;
    emit_schedules_changed(&app);
    Ok(())
}

// Click-through from the schedules panel: returns the session whose chat the
// UI opens (the schedule's own, or a fresh one titled after the schedule);
// "sessions-changed" makes a freshly created session appear in the sidebar.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleOpenResult {
    session_id: i64,
}

#[tauri::command]
fn schedule_open(app: AppHandle, state: State<AppState>, id: i64) -> Result<ScheduleOpenResult> {
    let session_id = with_db(&state, |conn| scheduler::open_session(conn, id))?;
    let _ = app.emit("sessions-changed", serde_json::json!({}));
    Ok(ScheduleOpenResult { session_id })
}

// Global memory CRUD for the UI (SPEC 6.3/6.5); mutations notify listeners
// with "memory-changed" so the memory list and open chats can refresh.
#[tauri::command]
fn memory_list(state: State<AppState>) -> Result<Vec<memory::MemoryDto>> {
    with_db(&state, memory::list)
}

#[tauri::command]
fn memory_add(
    app: AppHandle,
    state: State<AppState>,
    content: String,
    project_id: Option<i64>,
    tags: Option<Vec<String>>,
) -> Result<memory::MemoryDto> {
    let dto = with_db(&state, |conn| memory::add(conn, &content, project_id, tags, Some("user")))?;
    emit_memory_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
fn memory_update(
    app: AppHandle,
    state: State<AppState>,
    id: i64,
    content: String,
    tags: Option<Vec<String>>,
) -> Result<memory::MemoryDto> {
    let dto = with_db(&state, |conn| memory::update(conn, id, &content, tags))?;
    emit_memory_changed(&app, Some(dto.id));
    Ok(dto)
}

#[tauri::command]
fn memory_delete(app: AppHandle, state: State<AppState>, id: i64) -> Result<()> {
    with_db(&state, |conn| memory::delete(conn, id))?;
    emit_memory_changed(&app, None);
    Ok(())
}

#[tauri::command]
fn memory_search(
    state: State<AppState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<memory::MemoryHit>> {
    let limit = limit.unwrap_or(10).clamp(1, 100);
    with_db(&state, |conn| memory::search(conn, &query, limit))
}

// Backfill the memory FTS mirror and vectors for records whose index rows
// are missing (legacy rows or vectors skipped while the model was absent).
#[tauri::command]
fn index_memory(state: State<AppState>) -> Result<usize> {
    memory::index_memory(&state)
}

// Per-server MCP status for the settings UI (SPEC 6.2; the UI itself is a
// later slice).
#[tauri::command]
fn mcp_status(state: State<AppState>) -> Vec<mcp::McpServerStatus> {
    lock_recover(&state.mcp).status()
}

// Re-reads mcp.json and restarts the servers; a server that fails to start
// keeps its error in the status rows instead of breaking the app.
#[tauri::command]
fn mcp_reload(state: State<AppState>) -> Result<Vec<mcp::McpServerStatus>> {
    let wd = state.working_dir()?;
    let mut registry = lock_recover(&state.mcp);
    registry.shutdown();
    *registry = mcp::McpRegistry::start(&wd);
    Ok(registry.status())
}

// Skills management (SPEC 6.2); the skills UI screen is the next slice.
#[tauri::command]
fn skills_list(state: State<AppState>) -> Result<Vec<skills::SkillInfo>> {
    Ok(skills::list(&state.working_dir()?))
}

#[tauri::command]
fn skills_set_enabled(
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
async fn skills_import(
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
async fn skills_install_builtin(
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
fn skills_create(
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

// Operational commands (SPEC 3.6, 5.2, 5.3); the settings UI lands next slice.

// One .zip with a consistent DB snapshot + Projects/ copy. Blocking zip work
// runs on the blocking pool with its own DB connection (same pattern as the
// embed-model download).
#[tauri::command]
async fn create_backup(state: State<'_, AppState>, dest: Option<String>) -> Result<ops::BackupInfo> {
    let wd = state.working_dir()?;
    let dest = dest.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&paths::db_path(&wd))?;
        ops::create_backup(&wd, &conn, dest.as_deref())
    })
    .await
    .map_err(|e| AppError::Io(format!("backup task: {e}")))?
}

#[tauri::command]
fn clear_tmp(state: State<AppState>) -> Result<ops::TmpStats> {
    ops::clear_tmp(&state.working_dir()?)
}

#[tauri::command]
fn backups_dir(state: State<AppState>) -> Result<String> {
    let dir = paths::backups_dir(&state.working_dir()?);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.display().to_string())
}

// Auto-backup state for the settings UI (SPEC 3.6): last backup time, the
// configured interval/keep, how many archives are on disk, and whether one is
// due now.
#[tauri::command]
fn backup_status(state: State<AppState>) -> Result<ops::BackupStatus> {
    ops::backup_status(&state.working_dir()?)
}

// Import/export (SPEC import/export slice): folders, .zip archives and
// Obsidian vaults into a project or the Notes vault; a project or the whole
// vault out to a .zip. Imports reindex the target best-effort and notify the
// UI (fs-changed / notes-changed / projects-changed).

// Best-effort reindex + UI notify after a project/notes import.
fn notify_import(
    app: &AppHandle,
    state: &AppState,
    wd: &std::path::Path,
    target: &str,
    project_id: Option<i64>,
    dest_rel: &str,
) {
    match target {
        "project" => {
            if let Some(pid) = project_id {
                let _ = index::index_project(state, pid);
                emit_fs_changed(app, pid, dest_rel);
                emit_projects_changed(app);
            }
        }
        "notes" => {
            let _ = index::index_notes(wd);
            emit_notes_changed(app, dest_rel);
        }
        _ => {}
    }
}

#[tauri::command]
fn import_folder(
    app: AppHandle,
    state: State<AppState>,
    target: String,
    project_id: Option<i64>,
    source_path: String,
    dest_rel: Option<String>,
) -> Result<importer::ImportSummary> {
    let wd = state.working_dir()?;
    let dest_rel = dest_rel.unwrap_or_default();
    let summary = with_db(&state, |conn| {
        importer::import_folder(conn, &wd, &target, project_id, &source_path, &dest_rel)
    })?;
    notify_import(&app, &state, &wd, &target, project_id, &dest_rel);
    Ok(summary)
}

#[tauri::command]
fn import_archive(
    app: AppHandle,
    state: State<AppState>,
    target: String,
    project_id: Option<i64>,
    source_path: String,
    dest_rel: Option<String>,
) -> Result<importer::ImportSummary> {
    let wd = state.working_dir()?;
    let dest_rel = dest_rel.unwrap_or_default();
    let summary = with_db(&state, |conn| {
        importer::import_archive(conn, &wd, &target, project_id, &source_path, &dest_rel)
    })?;
    notify_import(&app, &state, &wd, &target, project_id, &dest_rel);
    Ok(summary)
}

#[tauri::command]
fn import_obsidian(
    app: AppHandle,
    state: State<AppState>,
    source_path: String,
    dest_rel: Option<String>,
) -> Result<importer::ImportSummary> {
    let wd = state.working_dir()?;
    let dest_rel = dest_rel.unwrap_or_default();
    let summary = importer::import_obsidian(&wd, &source_path, &dest_rel)?;
    let _ = index::index_notes(&wd);
    emit_notes_changed(&app, dest_rel);
    Ok(summary)
}

// Blocking zip work runs on the blocking pool with its own DB connection
// (same pattern as create_backup).
#[tauri::command]
async fn export_project(
    state: State<'_, AppState>,
    project_id: i64,
    dest_path: Option<String>,
) -> Result<importer::ExportSummary> {
    let wd = state.working_dir()?;
    let dest = dest_path.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&paths::db_path(&wd))?;
        importer::export_project(&conn, &wd, project_id, dest.as_deref())
    })
    .await
    .map_err(|e| AppError::Io(format!("export task: {e}")))?
}

#[tauri::command]
async fn export_notes(
    state: State<'_, AppState>,
    dest_path: Option<String>,
) -> Result<importer::ExportSummary> {
    let wd = state.working_dir()?;
    let dest = dest_path.map(PathBuf::from);
    tauri::async_runtime::spawn_blocking(move || importer::export_notes(&wd, dest.as_deref()))
        .await
        .map_err(|e| AppError::Io(format!("export task: {e}")))?
}

// The OS plugin is the source of truth; a plugin/OS failure falls back to
// the app.json mirror instead of breaking the UI.
#[tauri::command]
fn get_autostart(app: AppHandle, state: State<AppState>) -> Result<bool> {
    use tauri_plugin_autostart::ManagerExt;
    match app.autolaunch().is_enabled() {
        Ok(enabled) => Ok(enabled),
        Err(_) => Ok(ops::autostart_choice(&state.working_dir()?)),
    }
}

#[tauri::command]
fn set_autostart(app: AppHandle, state: State<AppState>, enabled: bool) -> Result<bool> {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    ops::apply_autostart(
        &|on| {
            if on {
                autolaunch.enable()
            } else {
                autolaunch.disable()
            }
            .map_err(|e| e.to_string())
        },
        enabled,
    )?;
    ops::set_autostart_choice(&state.working_dir()?, enabled)?;
    Ok(enabled)
}

// Automatic backup (SPEC 3.6): runs on a background thread with its own DB
// connection so it never blocks startup, and a failure is only logged. The
// per-working-dir guard in ops::auto_backup collapses the startup and
// choose_working_dir triggers into a single run when they overlap.
fn spawn_auto_backup(wd: &std::path::Path) {
    let wd = wd.to_path_buf();
    std::thread::spawn(move || {
        let conn = match db::open(&paths::db_path(&wd)) {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("auto-backup open db failed: {e}");
                return;
            }
        };
        if let Err(e) = ops::auto_backup(&wd, &conn) {
            eprintln!("auto-backup failed: {e}");
        }
    });
}

// Opens the run journal and runs startup hygiene for a working dir: tmp
// auto-purge (SPEC 5.2) and journal retention pruning (SPEC 3.3), both
// driven by app.json and both best-effort.
fn open_journal(state: &AppState, wd: &std::path::Path) -> Result<()> {
    let cfg = ops::read_app_config(wd).unwrap_or_default();
    let _ = ops::cleanup_old_tmp(wd, cfg.tmp_retention_days);
    let journal = journal::Journal::open(&paths::logs_dir(wd))?;
    let _ = journal.prune_old(cfg.log_retention_days);
    *lock_recover(&state.journal) = Some(journal);
    Ok(())
}

// Menu-bar (tray) entry point (SPEC quick capture slice): a left click on the
// tray icon shows/focuses the main window; the menu routes «Быстрая заметка»
// and «Новая задача» to the frontend as "tray-action" events (the window is
// shown first), «Выход» exits natively.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrayAction {
    action: String,
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let open = MenuItem::with_id(app, "open", "Открыть Project-S", true, None::<&str>)?;
    let note = MenuItem::with_id(app, "quick-note", "Быстрая заметка", true, None::<&str>)?;
    let task = MenuItem::with_id(app, "new-task", "Новая задача", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &note,
            &task,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            match event.id.as_ref() {
                "quit" => app.exit(0),
                action @ ("open" | "quick-note" | "new-task") => {
                    show_main_window(app);
                    let _ = app.emit(
                        "tray-action",
                        TrayAction {
                            action: action.into(),
                        },
                    );
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

// Global CmdOrCtrl+Shift+Space (SUPER on macOS, CTRL elsewhere): shows the
// window and asks the frontend to open the capture modal. A registration
// failure (the shortcut is already taken) is logged, never fatal.
fn setup_global_shortcut(app: &AppHandle) {
    use tauri_plugin_global_shortcut::{
        Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
    };

    for modifiers in [
        Modifiers::SUPER | Modifiers::SHIFT,
        Modifiers::CONTROL | Modifiers::SHIFT,
    ] {
        let shortcut = Shortcut::new(Some(modifiers), Code::Space);
        let result = app.global_shortcut().on_shortcut(shortcut, |app, _gs, event| {
            if event.state == ShortcutState::Pressed {
                show_main_window(app);
                let _ = app.emit("quick-capture", serde_json::json!({}));
            }
        });
        if let Err(e) = result {
            eprintln!("global shortcut {shortcut:?} registration failed: {e}");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        // SPEC 5.3: "запускаться с системой" as a macOS LaunchAgent.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Quick capture global shortcut (registered in setup below).
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(AppState::new())
        .manage(Arc::new(AgentRegistry::new()))
        .setup(|app| {
            let app_cfg = app_config_dir(app.handle())?;
            let state = app.state::<AppState>();
            // Background emitters (the rename_chat, todowrite tools) reach
            // the windows through the app state handle.
            *lock_recover(&state.emitter) = Some(Arc::new(app.handle().clone()));
            if let Some(wd) = onboarding::load_working_dir(&app_cfg) {
                *lock_recover(&state.db) = Some(db::open(&paths::db_path(&wd))?);
                *lock_recover(&state.working_dir) = Some(wd.clone());
                // One-time indexing-limits migration for installs created
                // before the 10 MB / 1M chars defaults. Best-effort.
                let _ = config::migrate_indexing_defaults(&wd);
                // SPEC 6.2: keep the bundled builtin skill set in sync with
                // this app build on every start (missing/changed copies are
                // refreshed; user skills and unknown builtin dirs survive).
                // Best-effort: a sync failure must never block startup.
                let _ = skills::sync_builtin(&wd);
                // SPEC 5.1: drop harness sessions idle past the TTL on start.
                let ttl = session_ttl_days(&wd);
                let guard = lock_recover(&state.db);
                if let Some(conn) = guard.as_ref() {
                    let _ = harness::purge_inactive_sessions(conn, ttl);
                    // Background process rows from a previous run: the
                    // processes themselves were reparented to launchd and
                    // keep running; reconcile only updates their status,
                    // it never kills them.
                    let _ = processes::reconcile(conn);
                    // Trash/version retention (SPEC trash/versions slice):
                    // drop entries older than 30 days, best-effort.
                    reliability::startup_purge(conn, &wd);
                }
                drop(guard);
                // Notes vault: create/seed Notes/ before the watcher attaches
                // to it, then index it so notes are searchable right away.
                // Best-effort: a vault failure must never block startup.
                let _ = notes::ensure(&wd);
                open_journal(&state, &wd)?;
                watcher::start(app.handle(), &state, &wd);
                let _ = index::index_notes(&wd);
                *lock_recover(&state.mcp) = mcp::McpRegistry::start(&wd);
                scheduler::start(app.handle().clone());
                search_reindex::start_timer(app.handle().clone());
                spawn_auto_backup(&wd);
            }
            // Quick capture entry points: menu-bar item + global shortcut.
            setup_tray(app.handle())?;
            setup_global_shortcut(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping,
            onboarding_status,
            default_working_dir,
            choose_working_dir,
            get_working_dir,
            config_dir_path,
            read_config,
            write_config,
            list_projects,
            create_project,
            rename_project,
            project_path,
            touch_project_opened,
            project_brief,
            refresh_project_brief,
            move_project,
            set_task_columns,
            tasks_list,
            task_create,
            task_update,
            task_move,
            task_delete,
            link_create,
            todos_list,
            objective_create,
            objective_list,
            objective_update,
            objective_step_done,
            objective_delete,
            fs_list_dir,
            fs_read_text,
            fs_write_text,
            fs_create_folder,
            fs_create_file,
            fs_rename,
            fs_copy_as,
            fs_absolute_path,
            fs_reveal_in_finder,
            fs_extract_archive,
            fs_delete,
            fs_move,
            fs_import,
            fs_open_external,
            list_open_with_apps,
            get_open_with,
            remember_open_with,
            open_with,
            fs_read_binary_data_url,
            notes_ensure,
            notes_tree,
            notes_list,
            notes_read,
            notes_write,
            notes_create,
            notes_mkdir,
            notes_rename,
            notes_move,
            notes_copy,
            notes_delete,
            undo_last,
            undo_list,
            quick_capture,
            templates_list,
            template_create_project,
            template_create_task,
            template_create_note,
            template_document,
            trash_list,
            trash_restore,
            trash_purge,
            version_list,
            version_restore,
            ensure_notes_chat,
            notes_chat_history,
            send_notes_message,
            cancel_notes_run,
            search_notes,
            index_notes,
            attach_files,
            attachment_data_url,
            attachment_open,
            chat_attachment_data_url,
            chat_attachment_open,
            index_project,
            reindex_all,
            index_status,
            reindex_start,
            reindex_status,
            embed_status,
            download_embed_model,
            semantic_search,
            search_project,
            search_chats,
            global_search,
            index_chat_messages,
            list_models,
            test_connection,
            detect_vision,
            get_vision_status,
            send_project_message,
            resolve_confirmation,
            project_chat_history,
            cancel_run,
            harness_create_session,
            harness_list_sessions,
            harness_rename_session,
            harness_delete_session,
            harness_session_history,
            chat_pin,
            chat_truncate_after,
            chat_branch,
            chat_export,
            chat_search,
            send_session_message,
            cancel_session_run,
            purge_sessions,
            schedule_list,
            schedule_create,
            schedule_pause,
            schedule_resume,
            schedule_delete,
            schedule_open,
            memory_list,
            memory_add,
            memory_update,
            memory_delete,
            memory_search,
            index_memory,
            mcp_status,
            mcp_reload,
            skills_list,
            skills_set_enabled,
            skills_import,
            skills_install_builtin,
            skills_create,
            create_backup,
            clear_tmp,
            backups_dir,
            backup_status,
            import_folder,
            import_archive,
            import_obsidian,
            export_project,
            export_notes,
            get_autostart,
            set_autostart
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    // Clean MCP shutdown on app exit; stdio transports also kill their
    // children on drop as a safety net.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(state) = app_handle.try_state::<AppState>() {
                lock_recover(&state.mcp).shutdown();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // S1.4: a panic while a thread holds the DB mutex must not cascade into
    // every later command: with_db recovers the poisoned lock and still runs
    // the closure against the live connection (WAL + busy_timeout keep the
    // recovered SQLite connection safe to reuse).
    #[test]
    fn with_db_recovers_after_the_db_mutex_is_poisoned() {
        let wd = onboarding::temp_test_dir("poison-with-db");
        let state = AppState::new();
        *lock_recover(&state.db) = Some(db::open(&paths::db_path(&wd)).unwrap());
        let db = &state.db;
        std::thread::scope(|s| {
            let panicker = s.spawn(|| {
                let _guard = db.lock().unwrap();
                panic!("boom while holding the DB lock");
            });
            assert!(panicker.join().is_err());
        });
        assert!(state.db.is_poisoned());
        let tasks = with_db(&state, |conn| {
            let n: i64 = conn.query_row("SELECT count(*) FROM tasks", [], |r| r.get(0))?;
            Ok(n)
        })
        .unwrap();
        assert_eq!(tasks, 0);
        std::fs::remove_dir_all(&wd).ok();
    }
}
