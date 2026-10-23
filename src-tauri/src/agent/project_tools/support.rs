// Shared plumbing for the project tool set: project/working-dir/state accessors,
// the shared UI connection borrow, the UI change-event emitters, and the
// domain-error → envelope mapping. The facade re-exports the externally used
// helpers unchanged, so `agent::project_tools::…` import paths keep working.
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::ToolCtx;
use crate::error::{AppError, Result};
use crate::sync_util::lock_recover;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn require_project(ctx: &ToolCtx) -> Result<i64> {
    ctx.project_id
        .ok_or_else(|| AppError::Invalid("no project bound to this agent run".into()))
}

pub(crate) fn require_wd(ctx: &ToolCtx) -> Result<PathBuf> {
    ctx.working_dir.clone().ok_or(AppError::NotConfigured)
}

pub(crate) fn state_of(ctx: &ToolCtx) -> Result<&crate::state::AppState> {
    ctx.state.as_deref().ok_or(AppError::NotConfigured)
}

// Borrows the shared UI connection for one domain call.
pub(crate) fn with_conn<T>(ctx: &ToolCtx, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let guard = lock_recover(&state_of(ctx)?.db);
    f(guard.as_ref().ok_or(AppError::NotConfigured)?)
}

// Best-effort UI notify through the run state's emitter (same path as
// todowrite's "todos-changed"): the open kanban must follow every task
// mutation, whether it came from a command or an agent tool; a missing
// emitter is not an error.
pub(crate) fn emit_tasks_changed(ctx: &ToolCtx, project_id: i64) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("tasks-changed", json!({ "projectId": project_id }));
        }
    }
}

// Trash panel refresh after a reversible delete from any agent tool.
pub(crate) fn emit_trash_changed(ctx: &ToolCtx, id: Option<i64>) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("trash-changed", json!({ "id": id }));
        }
    }
}

pub(crate) fn emit_projects_changed(ctx: &ToolCtx) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("projects-changed", json!({}));
        }
    }
}

// Version history panel refresh after a snapshot was saved.
pub(crate) fn emit_versions_changed(ctx: &ToolCtx, scope: &str, path: &str) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event(
                "versions-changed",
                json!({ "scope": scope, "path": path }),
            );
        }
    }
}

// Reload an open text preview after an agent write: same payload as the
// fs-changed event of the UI commands (commands/mod.rs).
pub(crate) fn emit_fs_changed(ctx: &ToolCtx, project_id: i64, rel_path: impl Into<String>) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event(
                "fs-changed",
                json!({ "projectId": project_id, "relPath": rel_path.into() }),
            );
        }
    }
}

// (projectId, project-relative path) of the project folder containing the
// written absolute path; None when the write lies outside every project, so
// no misleading project event is emitted.
pub(crate) fn project_for_abs(ctx: &ToolCtx, abs: &Path) -> Option<(i64, String)> {
    let state = state_of(ctx).ok()?;
    let guard = lock_recover(&state.db);
    let conn = guard.as_ref()?;
    let mut stmt = conn.prepare("SELECT id, path FROM projects").ok()?;
    let roots: Vec<(i64, PathBuf)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, PathBuf::from(r.get::<_, String>(1)?))))
        .ok()?
        .flatten()
        .collect();
    let target = crate::path_policy::canonicalish(abs);
    roots.into_iter().find_map(|(id, root)| {
        let rest = target
            .strip_prefix(&crate::path_policy::canonicalish(&root))
            .ok()?;
        (!rest.as_os_str().is_empty()).then(|| (id, rest.to_string_lossy().into_owned()))
    })
}

// fs-changed for a written absolute path, but only when it lies inside a
// project folder: the preview reload is project-scoped.
pub(crate) fn emit_fs_changed_for_abs(ctx: &ToolCtx, abs: &Path) {
    if let Some((id, rel)) = project_for_abs(ctx, abs) {
        emit_fs_changed(ctx, id, rel);
    }
}

// Maps a domain AppError to the envelope code (SPEC 6.4): missing entities are
// not_found, path escapes and symlink refusals are permission_denied, the rest
// (binary files, bad names, IO/DB faults) are execution_error.
pub fn fail(e: AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("not found") {
        ToolErrorCode::NotFound
    } else if msg.contains("not project-relative")
        || msg.contains("escapes the project")
        || msg.contains("symlink")
    {
        ToolErrorCode::PermissionDenied
    } else if msg.contains("ambiguous") {
        ToolErrorCode::InvalidArgs
    } else {
        ToolErrorCode::ExecutionError
    };
    ToolResult::err(code, msg)
}

pub(crate) fn envelope<T: serde::Serialize>(r: Result<T>) -> ToolResult {
    match r {
        Ok(v) => ToolResult::ok(serde_json::to_value(v).unwrap_or(Value::Null)),
        Err(e) => fail(e),
    }
}
