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
mod journal;
mod llm;
mod mcp;
mod memory;
mod notes;
mod onboarding;
mod ops;
mod paths;
mod processes;
mod projects;
mod scheduler;
mod search;
mod skills;
mod state;
mod tasks;
mod todos;
mod watcher;

use agent::permissions::ConfirmDecision;
use error::{AppError, Result};
use fs_ops::FsEntry;
use projects::ProjectDto;
use tasks::TaskDto;
use rusqlite::Connection;
use serde_json::Value;
use state::{AgentRegistry, AppState, RunKey};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

fn app_config_dir(app: &AppHandle) -> Result<PathBuf> {
    app.path()
        .app_config_dir()
        .map_err(|e| AppError::Io(e.to_string()))
}

fn with_db<T>(state: &AppState, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let guard = state.db.lock().unwrap();
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
    *state.db.lock().unwrap() = Some(conn);
    *state.working_dir.lock().unwrap() = Some(wd.clone());
    open_journal(&state, &wd)?;
    let _ = with_db(&state, processes::reconcile);
    watcher::start(&app, &state, &wd);
    let _ = index::index_notes(&wd);
    *state.mcp.lock().unwrap() = mcp::McpRegistry::start(&wd);
    scheduler::start(app.clone());
    Ok(info)
}

#[tauri::command]
fn get_working_dir(state: State<AppState>) -> Option<String> {
    state
        .working_dir
        .lock()
        .unwrap()
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
    with_db(&state, projects::list_projects)
}

#[tauri::command]
fn create_project(app: AppHandle, state: State<AppState>, name: String) -> Result<ProjectDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| projects::create_project(conn, &wd, &name))?;
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
    let dto = with_db(&state, |conn| projects::rename_project(conn, &wd, id, &new_name))?;
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

#[tauri::command]
fn task_create(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    title: String,
    status: Option<String>,
) -> Result<TaskDto> {
    let wd = state.working_dir()?;
    let dto = with_db(&state, |conn| {
        let dto = tasks::task_create(conn, project_id, &title, status)?;
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
        let dto = tasks::task_update(conn, id, patch)?;
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
        let dto = tasks::task_move(conn, id, &status, position)?;
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
        let project_id = tasks::task_delete(conn, id)?;
        export::export_project(conn, &wd, project_id)?;
        Ok(project_id)
    })?;
    emit_tasks_changed(&app, project_id);
    Ok(())
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
    let item = with_db(&state, |conn| {
        fs_ops::write_text(conn, &wd, project_id, &rel_path, &content)
    })?;
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
    let item = with_db(&state, |conn| {
        fs_ops::rename(conn, &wd, project_id, &rel_path, &new_name)
    })?;
    emit_fs_changed(&app, project_id, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn fs_duplicate(
    app: AppHandle,
    state: State<AppState>,
    project_id: i64,
    rel_path: String,
) -> Result<FsEntry> {
    let wd = state.working_dir()?;
    let item = with_db(&state, |conn| fs_ops::duplicate(conn, &wd, project_id, &rel_path))?;
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
    with_db(&state, |conn| fs_ops::delete(conn, &wd, project_id, &rel_path))?;
    emit_fs_changed(&app, project_id, rel_path);
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
    let item = with_db(&state, |conn| {
        fs_ops::move_entry(conn, &wd, project_id, &from_rel, &to_dir_rel)
    })?;
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
    let item = notes::write(&state.working_dir()?, &rel, &content)?;
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
    let item = notes::create(&state.working_dir()?, &rel, content.as_deref())?;
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
    let item = notes::rename(&state.working_dir()?, &rel, &new_name)?;
    emit_notes_changed(&app, item.rel_path.clone());
    Ok(item)
}

#[tauri::command]
fn notes_delete(app: AppHandle, state: State<AppState>, rel: String) -> Result<()> {
    notes::delete(&state.working_dir()?, &rel)?;
    emit_notes_changed(&app, rel);
    Ok(())
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
fn schedule_create(
    app: AppHandle,
    state: State<AppState>,
    kind: String,
    spec: String,
    prompt: Option<String>,
    timezone: Option<String>,
    session_id: Option<i64>,
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
) -> Result<Vec<memory::MemoryDto>> {
    let limit = limit.unwrap_or(10).clamp(1, 100);
    with_db(&state, |conn| memory::search(conn, &query, limit))
}

// Per-server MCP status for the settings UI (SPEC 6.2; the UI itself is a
// later slice).
#[tauri::command]
fn mcp_status(state: State<AppState>) -> Vec<mcp::McpServerStatus> {
    state.mcp.lock().unwrap().status()
}

// Re-reads mcp.json and restarts the servers; a server that fails to start
// keeps its error in the status rows instead of breaking the app.
#[tauri::command]
fn mcp_reload(state: State<AppState>) -> Result<Vec<mcp::McpServerStatus>> {
    let wd = state.working_dir()?;
    let mut registry = state.mcp.lock().unwrap();
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

// Run journal feed (SPEC 3.3): read-only scan of Settings/logs/runs-*.jsonl
// with filters; also returns the day/tool options for the "Журнал" UI.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn journal_list(
    state: State<AppState>,
    days: Option<u32>,
    kind: Option<String>,
    tool: Option<String>,
    ok: Option<bool>,
    project_id: Option<i64>,
    session_id: Option<i64>,
    limit: Option<usize>,
) -> Result<journal::JournalListResult> {
    let wd = state.working_dir()?;
    let filter = journal::JournalFilter {
        days: days.unwrap_or(journal::DEFAULT_DAYS).clamp(1, 365),
        kind,
        tool,
        ok,
        project_id,
        session_id,
        limit,
    };
    Ok(journal::JournalListResult {
        entries: journal::list_entries(&wd, &filter)?,
        days: journal::journal_days(&wd)?,
        tools: journal::journal_tools(&wd, filter.days)?,
    })
}

// Opens the run journal and runs startup hygiene for a working dir: tmp
// auto-purge (SPEC 5.2) and journal retention pruning (SPEC 3.3), both
// driven by app.json and both best-effort.
fn open_journal(state: &AppState, wd: &std::path::Path) -> Result<()> {
    let cfg = ops::read_app_config(wd).unwrap_or_default();
    let _ = ops::cleanup_old_tmp(wd, cfg.tmp_retention_days);
    let journal = journal::Journal::open(&paths::logs_dir(wd))?;
    let _ = journal.prune_old(cfg.log_retention_days);
    *state.journal.lock().unwrap() = Some(journal);
    Ok(())
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
        .manage(AppState::new())
        .manage(Arc::new(AgentRegistry::new()))
        .setup(|app| {
            let app_cfg = app_config_dir(app.handle())?;
            let state = app.state::<AppState>();
            // Background emitters (the rename_chat, todowrite tools) reach
            // the windows through the app state handle.
            *state.emitter.lock().unwrap() = Some(Arc::new(app.handle().clone()));
            if let Some(wd) = onboarding::load_working_dir(&app_cfg) {
                *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd))?);
                *state.working_dir.lock().unwrap() = Some(wd.clone());
                // SPEC 6.2: keep the bundled builtin skill set in sync with
                // this app build on every start (missing/changed copies are
                // refreshed; user skills and unknown builtin dirs survive).
                // Best-effort: a sync failure must never block startup.
                let _ = skills::sync_builtin(&wd);
                // SPEC 5.1: drop harness sessions idle past the TTL on start.
                let ttl = session_ttl_days(&wd);
                let guard = state.db.lock().unwrap();
                if let Some(conn) = guard.as_ref() {
                    let _ = harness::purge_inactive_sessions(conn, ttl);
                    // Background process rows from a previous run: the
                    // processes themselves were reparented to launchd and
                    // keep running; reconcile only updates their status,
                    // it never kills them.
                    let _ = processes::reconcile(conn);
                }
                drop(guard);
                // Notes vault: create/seed Notes/ before the watcher attaches
                // to it, then index it so notes are searchable right away.
                // Best-effort: a vault failure must never block startup.
                let _ = notes::ensure(&wd);
                open_journal(&state, &wd)?;
                watcher::start(app.handle(), &state, &wd);
                let _ = index::index_notes(&wd);
                *state.mcp.lock().unwrap() = mcp::McpRegistry::start(&wd);
                scheduler::start(app.handle().clone());
            }
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
            tasks_list,
            task_create,
            task_update,
            task_move,
            task_delete,
            todos_list,
            fs_list_dir,
            fs_read_text,
            fs_write_text,
            fs_create_folder,
            fs_create_file,
            fs_rename,
            fs_duplicate,
            fs_delete,
            fs_move,
            fs_import,
            fs_open_external,
            fs_read_binary_data_url,
            notes_ensure,
            notes_tree,
            notes_list,
            notes_read,
            notes_write,
            notes_create,
            notes_rename,
            notes_delete,
            ensure_notes_chat,
            notes_chat_history,
            send_notes_message,
            cancel_notes_run,
            search_notes,
            index_notes,
            attach_files,
            attachment_data_url,
            attachment_open,
            index_project,
            reindex_all,
            index_status,
            embed_status,
            download_embed_model,
            semantic_search,
            search_project,
            search_chats,
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
            send_session_message,
            cancel_session_run,
            purge_sessions,
            schedule_list,
            schedule_create,
            schedule_pause,
            schedule_resume,
            schedule_delete,
            memory_list,
            memory_add,
            memory_update,
            memory_delete,
            memory_search,
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
            get_autostart,
            set_autostart,
            journal_list
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    // Clean MCP shutdown on app exit; stdio transports also kill their
    // children on drop as a safety net.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(state) = app_handle.try_state::<AppState>() {
                state.mcp.lock().unwrap().shutdown();
            }
        }
    });
}
