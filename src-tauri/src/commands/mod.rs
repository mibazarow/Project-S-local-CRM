pub(crate) mod attachments;
pub(crate) mod capture;
pub(crate) mod chat;
pub(crate) mod files;
pub(crate) mod harness;
pub(crate) mod index;
pub(crate) mod links;
pub(crate) mod llm;
pub(crate) mod mcp;
pub(crate) mod memory;
pub(crate) mod notes;
pub(crate) mod objectives;
pub(crate) mod onboarding;
pub(crate) mod ops;
pub(crate) mod projects;
pub(crate) mod schedules;
pub(crate) mod search;
pub(crate) mod settings;
pub(crate) mod skills;
pub(crate) mod style;
pub(crate) mod tasks;
pub(crate) mod templates;
pub(crate) mod trash;
pub(crate) mod undo;

use crate::error::{AppError, Result};
use crate::state::AppState;
use crate::sync_util::lock_recover;
use rusqlite::Connection;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

pub(crate) fn app_config_dir(app: &AppHandle) -> Result<PathBuf> {
    app.path()
        .app_config_dir()
        .map_err(|e| AppError::Io(e.to_string()))
}

pub(crate) fn with_db<T>(state: &AppState, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let guard = lock_recover(&state.db);
    f(guard.as_ref().ok_or(AppError::NotConfigured)?)
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FsChanged {
    project_id: i64,
    rel_path: String,
}

pub(crate) fn emit_fs_changed(app: &AppHandle, project_id: i64, rel_path: impl Into<String>) {
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

pub(crate) fn emit_tasks_changed(app: &AppHandle, project_id: i64) {
    let _ = app.emit("tasks-changed", TasksChanged { project_id });
}

// Objectives (Today panel goals with steps): every mutation reloads the list.
pub(crate) fn emit_objectives_changed(app: &AppHandle, id: Option<i64>) {
    let _ = app.emit(
        "objectives-changed",
        crate::objectives::ObjectivesChanged { id },
    );
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryChanged {
    id: Option<i64>,
}

pub(crate) fn emit_memory_changed(app: &AppHandle, id: Option<i64>) {
    let _ = app.emit("memory-changed", MemoryChanged { id });
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrashChanged {
    id: Option<i64>,
}

pub(crate) fn emit_trash_changed(app: &AppHandle, id: Option<i64>) {
    let _ = app.emit("trash-changed", TrashChanged { id });
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionsChanged {
    scope: String,
    path: String,
}

pub(crate) fn emit_versions_changed(app: &AppHandle, scope: &str, path: &str) {
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

pub(crate) fn emit_schedules_changed(app: &AppHandle) {
    let _ = app.emit("schedules-changed", SchedulesChanged {});
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillsChanged {}

pub(crate) fn emit_skills_changed(app: &AppHandle) {
    let _ = app.emit("skills-changed", SkillsChanged {});
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectsChanged {}

pub(crate) fn emit_projects_changed(app: &AppHandle) {
    let _ = app.emit("projects-changed", ProjectsChanged {});
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NotesChanged {
    rel_path: String,
}

pub(crate) fn emit_notes_changed(app: &AppHandle, rel_path: impl Into<String>) {
    let _ = app.emit(
        "notes-changed",
        NotesChanged {
            rel_path: rel_path.into(),
        },
    );
}

pub(crate) fn session_ttl_days(wd: &std::path::Path) -> u32 {
    crate::config::read_config(wd, crate::config::ConfigKind::App)
        .ok()
        .and_then(|v| serde_json::from_value::<crate::config::AppConfig>(v).ok())
        .map(|c| c.session_ttl_days)
        .unwrap_or(30)
}

// Automatic backup (SPEC 3.6): runs on a background thread with its own DB
// connection so it never blocks startup, and a failure is only logged. The
// per-working-dir guard in crate::ops::auto_backup collapses the startup and
// choose_working_dir triggers into a single run when they overlap.
pub(crate) fn spawn_auto_backup(wd: &std::path::Path) {
    let wd = wd.to_path_buf();
    std::thread::spawn(move || {
        let conn = match crate::db::open(&crate::paths::db_path(&wd)) {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("auto-backup open db failed: {e}");
                return;
            }
        };
        if let Err(e) = crate::ops::auto_backup(&wd, &conn) {
            eprintln!("auto-backup failed: {e}");
        }
    });
}

// Opens the run journal and runs startup hygiene for a working dir: tmp
// auto-purge (SPEC 5.2) and journal retention pruning (SPEC 3.3), both
// driven by app.json and both best-effort.
pub(crate) fn open_journal(state: &AppState, wd: &std::path::Path) -> Result<()> {
    let cfg = crate::ops::read_app_config(wd).unwrap_or_default();
    let _ = crate::ops::cleanup_old_tmp(wd, cfg.tmp_retention_days);
    let journal = crate::journal::Journal::open(&crate::paths::logs_dir(wd))?;
    let _ = journal.prune_old(cfg.log_retention_days);
    *lock_recover(&state.journal) = Some(journal);
    Ok(())
}
