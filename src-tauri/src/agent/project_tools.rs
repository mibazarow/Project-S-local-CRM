// Project-agent tool set (SPEC 4.6): search/read/write inside the project
// folder, task CRUD, and datas.md maintenance, on top of the agent core.
// Every path argument is resolved strictly inside the project root by the
// fs_ops resolvers; absolute OS paths are never exposed to the model.
use super::catalog::Catalog;
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::{extract, fs_ops, paths, search, tasks, undo};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

// Envelope headroom for extracted documents: the JSON wrapper around the
// content (keys, path, escaping) costs a little, so the extracted text is
// capped below max_result_len to keep the capped envelope inside the limit.
const EXTRACT_ENVELOPE_RESERVE: usize = 512;

// All project tools are core: the project agent's whole tool set is active
// from the first round (lazy discovery stays for the harness agent, 5.2).
pub fn project_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::agent::finish::FinishTaskTool),
        Arc::new(SearchTool),
        Arc::new(ReadFileTool),
        Arc::new(ListTreeTool),
        Arc::new(WriteFileTool),
        // Precise edits: exact string replacement and unified-diff patches
        // via git apply; see edit_tools.rs for the safety rules.
        Arc::new(crate::agent::edit_tools::EditFileTool::project()),
        Arc::new(crate::agent::edit_tools::ApplyPatchTool),
        // Native document generation (DOCX/PDF/MD/TXT/HTML + XLSX/CSV) with
        // pure Rust writers; see doc_tools.rs for the safety rules.
        Arc::new(crate::agent::doc_tools::CreateDocumentTool::project()),
        Arc::new(crate::agent::doc_tools::CreateSpreadsheetTool::project()),
        // OCR of images and PDFs via the macOS Vision framework through an
        // embedded Swift helper; see ocr_tools.rs for the safety rules.
        Arc::new(crate::agent::ocr_tools::OcrTool),
        // PDF merge/split/fill-form via Apple PDFKit through an embedded Swift
        // helper; see pdf_tools.rs for the safety rules.
        Arc::new(crate::agent::pdf_tools::PdfMergeTool),
        Arc::new(crate::agent::pdf_tools::PdfSplitTool),
        Arc::new(crate::agent::pdf_tools::PdfFillFormTool),
        // Image resize/crop/rotate/convert via the macOS sips CLI; in-place
        // calls confirm, new files inside the project skip; see image_tools.rs.
        Arc::new(crate::agent::image_tools::ImageTransformTool),
        Arc::new(CreateFolderTool),
        Arc::new(RenamePathTool),
        Arc::new(DeletePathTool),
        Arc::new(TaskListTool),
        Arc::new(TaskCreateTool),
        Arc::new(TaskUpdateTool),
        Arc::new(TaskMoveTool),
        Arc::new(TaskDeleteTool),
        Arc::new(DatasAddTool),
        Arc::new(DatasReadTool),
        // Git tools via the system git CLI (read-only core set + confirmed
        // mutating set); see git_tools.rs for the safety rules.
        Arc::new(crate::agent::git_tools::GitStatusTool),
        Arc::new(crate::agent::git_tools::GitDiffTool),
        Arc::new(crate::agent::git_tools::GitLogTool),
        Arc::new(crate::agent::git_tools::GitBranchTool),
        Arc::new(crate::agent::git_tools::GitCommitTool),
        Arc::new(crate::agent::git_tools::GitPullTool),
        Arc::new(crate::agent::git_tools::GitPushTool),
        // SSH tools via the system ssh/scp/ssh-keygen CLI: keys, ssh-agent
        // and askpass passwords, profiles from ssh.json and the project's
        // datas.md; every mutating call is confirmed; see ssh_tools.rs.
        Arc::new(crate::agent::ssh_tools::SshListProfilesTool),
        Arc::new(crate::agent::ssh_tools::SshExecTool),
        Arc::new(crate::agent::ssh_tools::SshCopyTool::upload()),
        Arc::new(crate::agent::ssh_tools::SshCopyTool::download()),
        Arc::new(crate::agent::ssh_tools::SshGenerateKeyTool),
        Arc::new(crate::agent::ssh_tools::SshInstallKeyTool),
        Arc::new(crate::agent::ssh_tools::SshSaveProfileToDatasTool),
        // macOS clipboard read/write via the system pbcopy/pbpaste; every
        // call hits the general pasteboard; see clipboard_tools.rs.
        Arc::new(crate::agent::clipboard_tools::ClipboardGetTool::default()),
        Arc::new(crate::agent::clipboard_tools::ClipboardSetTool::default()),
        Arc::new(crate::agent::net_tools::WebfetchTool::new()),
        Arc::new(crate::agent::net_tools::WebsearchTool::new()),
        Arc::new(crate::agent::net_tools::HttpRequestTool),
        Arc::new(crate::skills::SkillTool),
        Arc::new(crate::skills::CreateSkillTool),
        // Global memory is shared with the harness agent (SPEC 6.3).
        Arc::new(crate::memory::RememberTool),
        Arc::new(crate::memory::RecallTool),
        Arc::new(crate::memory::SetUserGenderTool),
        // Per-conversation plan panel (SPEC: todowrite/todoread).
        Arc::new(crate::todos::TodoWriteTool),
        Arc::new(crate::todos::TodoReadTool),
        // Agent-initiated chat attachment (SPEC 6.1): hand a project file to
        // the user as a new assistant turn.
        Arc::new(crate::agent::send_file::SendFileTool),
        // Bounded undo stack (SPEC undo slice): undo the newest reversible
        // file/note/task mutation made by commands or agent tools.
        Arc::new(crate::agent::undo_tools::UndoLastTool),
        Arc::new(crate::agent::undo_tools::UndoListTool),
    ];
    for tool in tools {
        // Names are statically unique; a failure here is a programming bug.
        catalog.register(tool, true).expect("unique tool names");
    }
    catalog
}

fn require_project(ctx: &ToolCtx) -> Result<i64> {
    ctx.project_id
        .ok_or_else(|| AppError::Invalid("no project bound to this agent run".into()))
}

pub(crate) fn require_wd(ctx: &ToolCtx) -> Result<std::path::PathBuf> {
    ctx.working_dir.clone().ok_or(AppError::NotConfigured)
}

pub(crate) fn state_of(ctx: &ToolCtx) -> Result<&crate::state::AppState> {
    ctx.state.as_deref().ok_or(AppError::NotConfigured)
}

// Borrows the shared UI connection for one domain call.
pub(crate) fn with_conn<T>(ctx: &ToolCtx, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let guard = state_of(ctx)?.db.lock().unwrap();
    f(guard.as_ref().ok_or(AppError::NotConfigured)?)
}

// Best-effort UI notify through the run state's emitter (same path as
// todowrite's "todos-changed"): the open kanban must follow every task
// mutation, whether it came from a command or an agent tool; a missing
// emitter is not an error.
pub(crate) fn emit_tasks_changed(ctx: &ToolCtx, project_id: i64) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = state.emitter.lock().unwrap().as_ref() {
            emitter.emit_event("tasks-changed", json!({ "projectId": project_id }));
        }
    }
}

// Trash panel refresh after a reversible delete from any agent tool.
pub(crate) fn emit_trash_changed(ctx: &ToolCtx, id: Option<i64>) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = state.emitter.lock().unwrap().as_ref() {
            emitter.emit_event("trash-changed", json!({ "id": id }));
        }
    }
}

// Version history panel refresh after a snapshot was saved.
pub(crate) fn emit_versions_changed(ctx: &ToolCtx, scope: &str, path: &str) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = state.emitter.lock().unwrap().as_ref() {
            emitter.emit_event(
                "versions-changed",
                json!({ "scope": scope, "path": path }),
            );
        }
    }
}

// Maps a domain AppError to the envelope code (SPEC 6.4): missing entities are
// not_found, path escapes and symlink refusals are permission_denied, the rest
// (binary files, bad names, IO/DB faults) are execution_error.
fn fail(e: AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("not found") {
        ToolErrorCode::NotFound
    } else if msg.contains("not project-relative")
        || msg.contains("escapes the project")
        || msg.contains("symlink")
    {
        ToolErrorCode::PermissionDenied
    } else {
        ToolErrorCode::ExecutionError
    };
    ToolResult::err(code, msg)
}

fn is_missing(e: &AppError) -> bool {
    matches!(e, AppError::Invalid(_)) && e.to_string().contains("not found")
}

pub(crate) fn envelope<T: serde::Serialize>(r: Result<T>) -> ToolResult {
    match r {
        Ok(v) => ToolResult::ok(serde_json::to_value(v).unwrap_or(Value::Null)),
        Err(e) => fail(e),
    }
}

// Task ids are global in SQLite; the project agent may only touch its own
// project's tasks, and a foreign id reports as not_found (no existence leak).
fn task_owner(conn: &Connection, id: i64) -> Result<i64> {
    conn.query_row(
        "SELECT project_id FROM tasks WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )
    .map_err(|_| AppError::Invalid(format!("task {id} not found")))
}

fn require_own_task(conn: &Connection, id: i64, pid: i64) -> Result<()> {
    if task_owner(conn, id)? == pid {
        Ok(())
    } else {
        Err(AppError::Invalid(format!("task {id} not found")))
    }
}

pub struct SearchTool;

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Search this project's index (FTS + semantic) and return hits with relPath, name, snippet and matchTypes."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "number" }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let state = state_of(ctx)?;
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as usize;
        match search::search_project(state, pid, query, limit) {
            Ok(hits) => Ok(ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len())),
            Err(e) => Ok(fail(e)),
        }
    }
}

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file inside the project by project-relative path. Text files are returned as-is; docx/xlsx/xlsm/pdf are extracted to text (extracted: true). Images and other binary files are rejected."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": ["rel_path"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        // Resolve strictly inside the project first (not_found / escape /
        // folder errors map through fail()); the canonical path is what the
        // document extractor reads.
        let path = match with_conn(ctx, |conn| fs_ops::resolve_existing(conn, &wd, pid, rel)) {
            Ok(path) => path,
            Err(e) => return Ok(fail(e)),
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if fs_ops::IMAGE_EXTS.contains(&ext.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                crate::agent::harness_tools::IMAGE_READ_HINT,
            ));
        }
        match extract::kind(&path) {
            Some("docx") | Some("xlsx") | Some("pdf") => {
                let max_chars = self.max_result_len() - EXTRACT_ENVELOPE_RESERVE;
                match extract::extract_text(&path, max_chars) {
                    Ok(content) => Ok(ToolResult::ok(json!({
                        "relPath": rel,
                        "content": content,
                        "extracted": true,
                        "format": ext,
                    }))
                    .capped(self.max_result_len())),
                    Err(e) => Ok(fail(e)),
                }
            }
            // Plain text (md/txt/csv/... and unknown non-image extensions):
            // read_text validates UTF-8 and rejects NUL bytes as before.
            _ => {
                let r = with_conn(ctx, |conn| fs_ops::read_text(conn, &wd, pid, rel));
                Ok(match r {
                    Ok(content) => {
                        ToolResult::ok(json!({ "relPath": rel, "content": content }))
                            .capped(self.max_result_len())
                    }
                    Err(e) => fail(e),
                })
            }
        }
    }
}

pub struct ListTreeTool;

impl Tool for ListTreeTool {
    fn name(&self) -> &str {
        "list_tree"
    }
    fn description(&self) -> &str {
        "List a project folder (empty rel_path = project root): name, kind, ext, size per entry."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args.get("rel_path").and_then(Value::as_str).unwrap_or("");
        let r = with_conn(ctx, |conn| fs_ops::list_dir(conn, &wd, pid, rel));
        Ok(match r {
            Ok(entries) => {
                ToolResult::ok(json!({ "relPath": rel, "entries": entries })).capped(self.max_result_len())
            }
            Err(e) => fail(e),
        })
    }
}

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file inside the project. The parent folder must already exist."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rel_path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["rel_path", "content"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        let content = args["content"].as_str().unwrap_or_default();
        let path = Path::new(rel);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Snapshot the previous content before an overwrite (version history);
        // a missing file stays unversioned, a binary read just skips the save.
        let previous: Option<(String, String)> =
            with_conn(ctx, |conn| fs_ops::resolve_existing(conn, &wd, pid, rel))
                .ok()
                .and_then(|p| {
                    std::fs::read_to_string(&p)
                        .ok()
                        .map(|c| (p.display().to_string(), c))
                });
        let attempt = with_conn(ctx, |conn| fs_ops::write_text(conn, &wd, pid, rel, content));
        let result = match attempt {
            // fs_ops::write_text only overwrites; create the file first when missing.
            Err(e) if is_missing(&e) => with_conn(ctx, |conn| {
                fs_ops::create_file(conn, &wd, pid, &parent, &name)?;
                fs_ops::write_text(conn, &wd, pid, rel, content)
            }),
            other => other,
        };
        if let Ok(entry) = &result {
            if let Some((abs, prev)) = &previous {
                let version_id = with_conn(ctx, |conn| {
                    crate::reliability::version_save(conn, &wd, "project", abs, prev)
                })
                .ok();
                if let Some(version_id) = version_id {
                    emit_versions_changed(ctx, "project", abs);
                    let _ = with_conn(ctx, |conn| {
                        undo::push_file_write(
                            conn,
                            "project",
                            abs,
                            Some(version_id),
                            false,
                            Some(pid),
                            Some(&entry.rel_path),
                        )
                    });
                }
            } else {
                let abs = with_conn(ctx, |conn| {
                    fs_ops::resolve_existing(conn, &wd, pid, &entry.rel_path)
                        .map(|p| p.display().to_string())
                })
                .ok();
                if let Some(abs) = abs {
                    let _ = with_conn(ctx, |conn| {
                        undo::push_file_write(
                            conn,
                            "project",
                            &abs,
                            None,
                            true,
                            Some(pid),
                            Some(&entry.rel_path),
                        )
                    });
                }
            }
        }
        Ok(match result {
            Ok(entry) => ToolResult::ok(json!({ "entry": entry })),
            Err(e) => fail(e),
        })
    }
}

pub struct CreateFolderTool;

impl Tool for CreateFolderTool {
    fn name(&self) -> &str {
        "create_folder"
    }
    fn description(&self) -> &str {
        "Create a folder inside the project under parent_rel (empty parent_rel = project root)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent_rel": { "type": "string" },
                "name": { "type": "string" }
            },
            "required": ["parent_rel", "name"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let parent = args["parent_rel"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("");
        let r = with_conn(ctx, |conn| fs_ops::create_folder(conn, &wd, pid, parent, name));
        Ok(envelope(r))
    }
}

pub struct RenamePathTool;

impl Tool for RenamePathTool {
    fn name(&self) -> &str {
        "rename_path"
    }
    fn description(&self) -> &str {
        "Rename a file or folder inside the project to new_name (stays in the same parent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "rel_path": { "type": "string" },
                "new_name": { "type": "string" }
            },
            "required": ["rel_path", "new_name"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        let new_name = args["new_name"].as_str().unwrap_or_default();
        let from_abs = with_conn(ctx, |conn| {
            fs_ops::resolve_existing(conn, &wd, pid, rel).map(|p| p.display().to_string())
        })
        .ok();
        let r = with_conn(ctx, |conn| fs_ops::rename(conn, &wd, pid, rel, new_name));
        if let (Ok(entry), Some(from_abs)) = (&r, from_abs) {
            let to_abs = with_conn(ctx, |conn| {
                fs_ops::resolve_existing(conn, &wd, pid, &entry.rel_path)
                    .map(|p| p.display().to_string())
            })
            .ok();
            if let Some(to_abs) = to_abs {
                let _ = with_conn(ctx, |conn| {
                    undo::push_rename(
                        conn,
                        "project",
                        &from_abs,
                        &to_abs,
                        Some(pid),
                        Some(&entry.rel_path),
                    )
                });
            }
        }
        Ok(envelope(r))
    }
}

pub struct DeletePathTool;

impl Tool for DeletePathTool {
    fn name(&self) -> &str {
        "delete_path"
    }
    fn description(&self) -> &str {
        "Delete a file or folder (recursively) inside the project. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "rel_path": { "type": "string" } },
            "required": ["rel_path"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let rel = args["rel_path"].as_str().unwrap_or_default();
        let path_abs = with_conn(ctx, |conn| {
            fs_ops::resolve_existing(conn, &wd, pid, rel).map(|p| p.display().to_string())
        })
        .ok();
        let r = with_conn(ctx, |conn| fs_ops::delete(conn, &wd, pid, rel));
        if let Ok(id) = &r {
            if let Some(path_abs) = path_abs {
                let _ = with_conn(ctx, |conn| {
                    undo::push_delete(conn, *id, "project", &path_abs, Some(pid), Some(rel))
                });
            }
            emit_trash_changed(ctx, Some(*id));
        }
        Ok(match r {
            Ok(id) => ToolResult::ok(json!({ "deleted": rel, "trashId": id })),
            Err(e) => fail(e),
        })
    }
}

pub struct TaskListTool;

impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "List this project's tasks in kanban order; optional status filter (new|progress|done)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["new", "progress", "done"] }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let status = args.get("status").and_then(Value::as_str);
        let r = with_conn(ctx, |conn| {
            let all = tasks::tasks_list(conn, pid)?;
            Ok::<Vec<tasks::TaskDto>, AppError>(
                all.into_iter()
                    .filter(|t| status.map_or(true, |s| t.status == s))
                    .collect(),
            )
        });
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "tasks": list })).capped(self.max_result_len()),
            Err(e) => fail(e),
        })
    }
}

pub struct TaskCreateTool;

impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }
    fn description(&self) -> &str {
        "Create a task in this project; optional status (new|progress|done, default new), priority (low|normal|high|urgent, default normal), recurrence (none|daily|weekly|monthly, default none), checklist ([{text, done?}], max 100 items)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "status": { "type": "string", "enum": ["new", "progress", "done"] },
                "priority": { "type": "string", "enum": ["low", "normal", "high", "urgent"] },
                "recurrence": { "type": "string", "enum": ["none", "daily", "weekly", "monthly"] },
                "checklist": {
                    "type": "array",
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" },
                            "done": { "type": "boolean" }
                        },
                        "required": ["text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let title = args["title"].as_str().unwrap_or_default();
        let status = args.get("status").and_then(Value::as_str).map(str::to_owned);
        let priority = args.get("priority").and_then(Value::as_str).map(str::to_owned);
        let recurrence = args.get("recurrence").and_then(Value::as_str).map(str::to_owned);
        let checklist = match parse_checklist_arg(args) {
            Ok(items) => items,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let r = with_conn(ctx, |conn| {
            tasks::task_create_with(conn, pid, title, status, priority, checklist, recurrence)
        });
        if let Ok(dto) = &r {
            let _ = with_conn(ctx, |conn| undo::push_task_create(conn, dto.id, pid));
            emit_tasks_changed(ctx, pid);
        }
        Ok(envelope(r))
    }
}

// Optional "checklist" argument as Vec<ChecklistItem>; absent -> None.
pub(crate) fn parse_checklist_arg(
    args: &Value,
) -> std::result::Result<Option<Vec<tasks::ChecklistItem>>, String> {
    match args.get("checklist") {
        None => Ok(None),
        Some(value) => serde_json::from_value::<Vec<tasks::ChecklistItem>>(value.clone())
            .map(Some)
            .map_err(|e| format!("checklist must be an array of {{text, done?}}: {e}")),
    }
}

pub struct TaskUpdateTool;

impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }
    fn description(&self) -> &str {
        "Update fields of one of this project's tasks; absent fields stay unchanged, explicit null clears comment/dueDate/remindAt. Dates: YYYY-MM-DD, remindAt: YYYY-MM-DDTHH:MM. priority: low|normal|high|urgent, recurrence: none|daily|weekly|monthly, checklist: [{text, done?}] max 100 (null/absent keeps the current value)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "number" },
                "title": { "type": "string" },
                "comment": { "type": ["string", "null"] },
                "dueDate": { "type": ["string", "null"] },
                "remindAt": { "type": ["string", "null"] },
                "tags": { "type": "array", "items": { "type": "string" } },
                "priority": { "type": "string", "enum": ["low", "normal", "high", "urgent"] },
                "recurrence": { "type": "string", "enum": ["none", "daily", "weekly", "monthly"] },
                "checklist": {
                    "type": "array",
                    "maxItems": 100,
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" },
                            "done": { "type": "boolean" }
                        },
                        "required": ["text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        // TaskPatch ignores the extra "id" key; its opt fields keep absent
        // fields unchanged and treat explicit null as "clear".
        let patch: tasks::TaskPatch = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string()));
            }
        };
        let r = with_conn(ctx, |conn| {
            require_own_task(conn, id, pid)?;
            let snapshot = tasks::task_snapshot(conn, id).ok();
            let dto = tasks::task_update(conn, id, patch)?;
            if let Some(snapshot) = snapshot {
                let _ = undo::push_task_snapshot(conn, "task_update", &snapshot, None);
            }
            Ok(dto)
        });
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
        }
        Ok(envelope(r))
    }
}

pub struct TaskMoveTool;

impl Tool for TaskMoveTool {
    fn name(&self) -> &str {
        "task_move"
    }
    fn description(&self) -> &str {
        "Move one of this project's tasks to a kanban column (new|progress|done); optional position, appended at the column end when absent. Moving a recurring task (recurrence daily/weekly/monthly) to done queues the next occurrence in the new column."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "number" },
                "status": { "type": "string", "enum": ["new", "progress", "done"] },
                "position": { "type": "number" }
            },
            "required": ["id", "status"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let status = args["status"].as_str().unwrap_or_default();
        let r = with_conn(ctx, |conn| {
            require_own_task(conn, id, pid)?;
            let snapshot = tasks::task_snapshot(conn, id).ok();
            let position = match args.get("position").and_then(Value::as_f64) {
                Some(p) => p,
                None => conn.query_row(
                    "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks WHERE project_id = ?1 AND status = ?2",
                    params![pid, status],
                    |row| row.get(0),
                )?,
            };
            let (dto, spawned) = tasks::task_move_capture(conn, id, status, position)?;
            if let Some(snapshot) = snapshot {
                let _ = undo::push_task_snapshot(conn, "task_move", &snapshot, spawned);
            }
            Ok(dto)
        });
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
        }
        Ok(envelope(r))
    }
}

pub struct TaskDeleteTool;

impl Tool for TaskDeleteTool {
    fn name(&self) -> &str {
        "task_delete"
    }
    fn description(&self) -> &str {
        "Delete one of this project's tasks. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "number" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| {
            require_own_task(conn, id, pid)?;
            let (project_id, trash_id) = tasks::task_delete_with_trash(conn, id)?;
            let _ = undo::push_task_delete(conn, trash_id, project_id);
            Ok(trash_id)
        });
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
            emit_trash_changed(ctx, None);
        }
        Ok(match r {
            Ok(_) => ToolResult::ok(json!({ "deleted": id })),
            Err(e) => fail(e),
        })
    }
}

// Structured datas.md edit: a section is "## <title>" up to the next "## "
// heading; an existing section is replaced in place, otherwise appended.
pub(crate) fn upsert_section(md: &str, title: &str, body: &str) -> (bool, String) {
    let heading = format!("## {title}");
    let section = format!("{heading}\n\n{}\n\n", body.trim());
    let mut out = String::new();
    let mut updated = false;
    let mut skipping = false;
    for line in md.lines() {
        if skipping {
            if line.starts_with("## ") {
                skipping = false;
            } else {
                continue;
            }
        }
        if !updated && line.trim_end() == heading {
            out.push_str(&section);
            updated = true;
            skipping = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !updated {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&section);
    }
    (updated, out)
}

// datas.md may be missing on hand-created project folders; treat it as empty.
pub(crate) fn read_datas(conn: &Connection, wd: &std::path::Path, pid: i64) -> Result<String> {
    match fs_ops::read_text(conn, wd, pid, paths::DATAS_MD) {
        Ok(text) => Ok(text),
        Err(e) if is_missing(&e) => Ok(String::new()),
        Err(e) => Err(e),
    }
}

pub(crate) fn write_datas(conn: &Connection, wd: &std::path::Path, pid: i64, content: &str) -> Result<fs_ops::FsEntry> {
    match fs_ops::write_text(conn, wd, pid, paths::DATAS_MD, content) {
        Err(e) if is_missing(&e) => {
            fs_ops::create_file(conn, wd, pid, "", paths::DATAS_MD)?;
            fs_ops::write_text(conn, wd, pid, paths::DATAS_MD, content)
        }
        other => other,
    }
}

pub struct DatasAddTool;

impl Tool for DatasAddTool {
    fn name(&self) -> &str {
        "datas_add"
    }
    fn description(&self) -> &str {
        "Add or update a \"## <title>\" section in the project's datas.md (access notes); an existing section is replaced, otherwise appended."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body": { "type": "string" }
            },
            "required": ["title", "body"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let title = args["title"].as_str().unwrap_or_default().trim().to_owned();
        if title.is_empty() || title.starts_with('#') || title.contains('\n') {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "title must be a single plain line"));
        }
        let body = args["body"].as_str().unwrap_or_default();
        let r = with_conn(ctx, |conn| {
            let current = read_datas(conn, &wd, pid)?;
            let (updated, new_md) = upsert_section(&current, &title, body);
            write_datas(conn, &wd, pid, &new_md).map(|entry| (updated, entry))
        });
        Ok(match r {
            Ok((updated, entry)) => ToolResult::ok(json!({ "title": title, "updated": updated, "entry": entry })),
            Err(e) => fail(e),
        })
    }
}

pub struct DatasReadTool;

impl Tool for DatasReadTool {
    fn name(&self) -> &str {
        "datas_read"
    }
    fn description(&self) -> &str {
        "Read the project's datas.md (access notes file); empty string when the file is missing."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let wd = require_wd(ctx)?;
        let r = with_conn(ctx, |conn| read_datas(conn, &wd, pid));
        Ok(match r {
            Ok(text) => {
                ToolResult::ok(json!({ "relPath": paths::DATAS_MD, "content": text })).capped(self.max_result_len())
            }
            Err(e) => fail(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Resolution;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::index;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::state::{AppState, UiEmitter};
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingEmitter {
        events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl UiEmitter for RecordingEmitter {
        fn emit_event(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_owned(), payload));
        }
    }

    struct Env {
        wd: PathBuf,
        ctx: ToolCtx,
        events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    fn setup(tag: &str) -> Env {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let recorder = RecordingEmitter::default();
        let events = recorder.events.clone();
        *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
        let ctx = ToolCtx {
            project_id: Some(dto.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        Env { wd, ctx, events }
    }

    fn root(wd: &Path) -> PathBuf {
        paths::projects_dir(wd).join("proj")
    }

    fn tool(name: &str) -> Arc<dyn Tool> {
        let catalog = project_catalog();
        match catalog.resolve(name) {
            Resolution::Found(t) => t.clone(),
            _ => panic!("tool {name} missing from project catalog"),
        }
    }

    fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        tool(name).execute(&args, ctx).unwrap()
    }

    const TOOL_NAMES: [&str; 53] = [
        "finish_task",
        "search",
        "read_file",
        "list_tree",
        "write_file",
        "edit_file",
        "apply_patch",
        "create_document",
        "create_spreadsheet",
        "ocr",
        "pdf_merge",
        "pdf_split",
        "pdf_fill_form",
        "image_transform",
        "create_folder",
        "rename_path",
        "delete_path",
        "task_list",
        "task_create",
        "task_update",
        "task_move",
        "task_delete",
        "datas_add",
        "datas_read",
        "git_status",
        "git_diff",
        "git_log",
        "git_branch",
        "git_commit",
        "git_pull",
        "git_push",
        "ssh_list_profiles",
        "ssh_exec",
        "ssh_upload",
        "ssh_download",
        "ssh_generate_key",
        "ssh_install_key",
        "ssh_save_profile_to_datas",
        "clipboard_get",
        "clipboard_set",
        "webfetch",
        "websearch",
        "http_request",
        "skill",
        "create_skill",
        "remember",
        "recall",
        "set_user_gender",
        "todowrite",
        "todoread",
        "send_file",
        "undo_last",
        "undo_list",
    ];

    #[test]
    fn catalog_registers_unique_tools_with_object_schemas() {
        let specs = project_catalog().to_specs();
        let mut names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "duplicate tool names in specs");
        names.sort();
        for expected in TOOL_NAMES {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        assert_eq!(specs.len(), TOOL_NAMES.len() + 2); // + discovery tools
        for spec in specs.iter().filter(|s| TOOL_NAMES.contains(&s.name.as_str())) {
            let p = &spec.parameters;
            assert_eq!(p["type"], "object", "{} schema type", spec.name);
            assert_eq!(p["additionalProperties"], false, "{} extras", spec.name);
            assert!(p["properties"].is_object(), "{} properties", spec.name);
            assert!(p["required"].is_array(), "{} required", spec.name);
        }
    }

    #[test]
    fn validator_accepts_good_call_and_rejects_bad_calls() {
        let schema = tool("write_file").parameters();
        assert!(validate_args(&schema, &json!({ "rel_path": "a.md", "content": "x" })).is_ok());
        let err = validate_args(&schema, &json!({ "rel_path": "a.md" })).unwrap_err();
        assert!(err.contains("content"), "{err}");
        let err = validate_args(&schema, &json!({ "rel_path": 1, "content": "x" })).unwrap_err();
        assert!(err.contains("string"), "{err}");
        let err = validate_args(&schema, &json!({ "rel_path": "a", "content": "x", "extra": 1 }))
            .unwrap_err();
        assert!(err.contains("extra"), "{err}");
    }

    #[test]
    fn search_read_and_list_round_trip() {
        let env = setup("pt-search");
        fs::create_dir_all(root(&env.wd).join("notes")).unwrap();
        fs::write(
            root(&env.wd).join("notes/plan.md"),
            "access data for the staging server example.ru",
        )
        .unwrap();
        index::index_project(env.ctx.state.as_deref().unwrap(), env.ctx.project_id.unwrap())
            .unwrap();

        let r = exec("search", json!({ "query": "staging" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits[0]["relPath"], "notes/plan.md");
        assert!(hits[0]["matchTypes"].as_array().unwrap().contains(&json!("fts")));
        assert!(hits[0]["snippet"].as_str().unwrap().contains("staging"));

        let r = exec("read_file", json!({ "rel_path": "notes/plan.md" }), &env.ctx);
        assert!(r.ok);
        assert!(r.data.unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("example.ru"));

        let r = exec("list_tree", json!({}), &env.ctx);
        assert!(r.ok);
        let entries = r.data.unwrap()["entries"].as_array().unwrap().clone();
        let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"notes"));
        assert!(names.contains(&"datas.md"));

        // binary files are refused with execution_error
        fs::write(root(&env.wd).join("bin.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec("read_file", json!({ "rel_path": "bin.dat" }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "execution_error");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in entries {
            zip.start_file(name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn docx_bytes(text: &str) -> Vec<u8> {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
        );
        zip_bytes(&[("word/document.xml", &xml)])
    }

    fn xlsx_bytes(text: &str) -> Vec<u8> {
        let shared = format!(
            r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1"><si><t>{text}</t></si></sst>"#
        );
        let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#;
        zip_bytes(&[("xl/sharedStrings.xml", &shared), ("xl/worksheets/sheet1.xml", sheet)])
    }

    #[test]
    fn read_file_extracts_documents_caps_and_rejects_images_and_binaries() {
        let env = setup("pt-docs");
        fs::create_dir_all(root(&env.wd).join("docs")).unwrap();

        // plain text keeps the old shape: no extracted/format keys
        fs::write(root(&env.wd).join("docs/plan.md"), "plain text body").unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/plan.md" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "plain text body");
        assert_eq!(data["relPath"], "docs/plan.md");
        assert!(data.get("extracted").is_none());
        assert!(data.get("format").is_none());

        // docx built in memory: extracted text plus the extracted/format keys
        fs::write(root(&env.wd).join("docs/report.docx"), docx_bytes("Hello docx world")).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/report.docx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Hello docx world");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "docx");
        assert_eq!(data["relPath"], "docs/report.docx");

        // xlsx likewise
        fs::write(root(&env.wd).join("docs/sheet.xlsx"), xlsx_bytes("Report Q")).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/sheet.xlsx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Report Q");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "xlsx");

        // images get the vision hint instead of base64
        fs::write(root(&env.wd).join("docs/pic.png"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/pic.png" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("vision-capable"), "{}", err.message);

        // unknown binary extensions keep the old clear rejection
        fs::write(root(&env.wd).join("docs/blob.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/blob.dat" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("binary"), "{}", err.message);

        // unsupported document extension (legacy .doc) is rejected too
        fs::write(root(&env.wd).join("docs/legacy.doc"), b"old binary format\x00").unwrap();
        let r = exec("read_file", json!({ "rel_path": "docs/legacy.doc" }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "execution_error");

        // max_chars cap: a long docx paragraph is cut to max_result_len
        fs::write(root(&env.wd).join("docs/long.docx"), docx_bytes(&"x".repeat(9_000))).unwrap();
        let r = ReadFileTool
            .execute(&json!({ "rel_path": "docs/long.docx" }), &env.ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(
            data["content"].as_str().unwrap().chars().count(),
            8_000 - EXTRACT_ENVELOPE_RESERVE
        );
        assert_eq!(data["extracted"], true);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn write_create_folder_rename_delete_round_trip() {
        let env = setup("pt-fs");
        let r = exec("create_folder", json!({ "parent_rel": "", "name": "docs" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["kind"], "dir");

        // create via write_file, then overwrite
        let r = exec(
            "write_file",
            json!({ "rel_path": "docs/note.md", "content": "v1" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["entry"]["relPath"], "docs/note.md");
        let r = exec(
            "write_file",
            json!({ "rel_path": "docs/note.md", "content": "v2" }),
            &env.ctx,
        );
        assert!(r.ok);
        assert_eq!(fs::read_to_string(root(&env.wd).join("docs/note.md")).unwrap(), "v2");

        let r = exec(
            "rename_path",
            json!({ "rel_path": "docs/note.md", "new_name": "renamed.md" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["relPath"], "docs/renamed.md");

        let r = exec("delete_path", json!({ "rel_path": "docs/renamed.md" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!root(&env.wd).join("docs/renamed.md").exists());

        // The overwrite snapshotted the previous content into the version
        // store (keyed by the canonical path), the delete landed in the
        // trash, and both panels got their change events.
        let state = env.ctx.state.clone().unwrap();
        let guard = state.db.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        let abs = root(&env.wd)
            .join("docs")
            .canonicalize()
            .unwrap()
            .join("note.md")
            .display()
            .to_string();
        assert_eq!(crate::reliability::version_list(conn, "project", &abs).unwrap().len(), 1);
        assert_eq!(crate::reliability::trash_list(conn).unwrap().len(), 1);
        drop(guard);
        let events: Vec<String> =
            env.events.lock().unwrap().iter().map(|(e, _)| e.clone()).collect();
        assert!(events.iter().any(|e| e == "versions-changed"), "{events:?}");
        assert!(events.iter().any(|e| e == "trash-changed"), "{events:?}");

        // missing file reads back as not_found
        let r = exec("read_file", json!({ "rel_path": "docs/renamed.md" }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn path_escapes_are_rejected_with_permission_denied() {
        let env = setup("pt-escape");
        for bad in ["../x", "/etc/passwd", "a/../../x", ".."] {
            for name in ["read_file", "write_file", "delete_path", "list_tree"] {
                let args = if name == "write_file" {
                    json!({ "rel_path": bad, "content": "x" })
                } else {
                    json!({ "rel_path": bad })
                };
                let r = exec(name, args, &env.ctx);
                assert!(!r.ok, "{name} accepted {bad:?}");
                assert_eq!(
                    r.error.unwrap().code,
                    "permission_denied",
                    "{name} on {bad:?}"
                );
            }
        }
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn task_tools_round_trip_and_project_scoping() {
        let env = setup("pt-tasks");
        let r = exec("task_create", json!({ "title": "  first  " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        let r = exec("task_list", json!({}), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);

        let r = exec(
            "task_update",
            json!({ "id": id, "title": "renamed", "comment": "note", "dueDate": "2099-01-02", "tags": ["a", ""] }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["title"], "renamed");
        assert_eq!(data["comment"], "note");
        assert_eq!(data["dueDate"], "2099-01-02");
        assert_eq!(data["tags"], json!(["a"]));

        // explicit null clears the comment
        let r = exec("task_update", json!({ "id": id, "comment": null }), &env.ctx);
        assert_eq!(r.data.unwrap()["comment"], Value::Null);

        let r = exec("task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.clone().unwrap()["status"], "done");
        assert!(r.data.unwrap()["closedAt"].as_str().is_some());

        let r = exec("task_list", json!({ "status": "done" }), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);
        let r = exec("task_list", json!({ "status": "new" }), &env.ctx);
        assert!(r.data.unwrap()["tasks"].as_array().unwrap().is_empty());

        // foreign task ids are not_found for this project
        let r = exec("task_delete", json!({ "id": 9999 }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        let r = exec("task_delete", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("task_list", json!({}), &env.ctx);
        assert!(r.data.unwrap()["tasks"].as_array().unwrap().is_empty());
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn task_tools_accept_and_validate_priority_checklist_recurrence() {
        let env = setup("pt-tasks-upgrade");
        let r = exec(
            "task_create",
            json!({
                "title": "report",
                "priority": "urgent",
                "recurrence": "weekly",
                "checklist": [{ "text": "draft", "done": true }, { "text": "send" }]
            }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let id = data["id"].as_i64().unwrap();
        assert_eq!(data["priority"], "urgent");
        assert_eq!(data["recurrence"], "weekly");
        assert_eq!(
            data["checklist"],
            json!([{ "text": "draft", "done": true }, { "text": "send", "done": false }])
        );

        let r = exec(
            "task_update",
            json!({ "id": id, "priority": "low", "recurrence": "none", "checklist": [{ "text": "only", "done": true }] }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(
            (data["priority"].as_str(), data["recurrence"].as_str()),
            (Some("low"), Some("none"))
        );
        assert_eq!(data["checklist"], json!([{ "text": "only", "done": true }]));

        // Domain validation surfaces as a tool error, not a panic.
        let r = exec("task_create", json!({ "title": "x", "priority": "critical" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("priority"));
        let r = exec("task_create", json!({ "title": "x", "recurrence": "yearly" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("recurrence"));
        let r = exec("task_update", json!({ "id": id, "checklist": [{ "nope": 1 }] }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // done move of a recurring task queues the next occurrence in 'new'.
        let r = exec(
            "task_update",
            json!({ "id": id, "recurrence": "daily", "dueDate": "2099-01-02" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let r = exec("task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("task_list", json!({ "status": "new" }), &env.ctx);
        let tasks = r.data.unwrap()["tasks"].as_array().unwrap().clone();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["title"], "report");
        assert_eq!(tasks[0]["dueDate"], "2099-01-03");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn task_mutations_emit_tasks_changed_for_the_bound_project() {
        let env = setup("pt-task-events");
        let pid = env.ctx.project_id.unwrap();
        let r = exec("task_create", json!({ "title": "live" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();
        exec("task_update", json!({ "id": id, "title": "renamed" }), &env.ctx);
        exec("task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        // a failed mutation must not notify
        exec("task_update", json!({ "id": 9999, "title": "ghost" }), &env.ctx);
        exec("task_delete", json!({ "id": id }), &env.ctx);

        let events = env.events.lock().unwrap().clone();
        let notified: Vec<&Value> = events
            .iter()
            .filter(|(name, _)| name == "tasks-changed")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(notified.len(), 4, "create/update/move/delete must notify: {events:?}");
        assert!(
            notified.iter().all(|p| p["projectId"] == json!(pid)),
            "payload must carry the bound project id: {notified:?}"
        );
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn datas_add_updates_sections_and_read_returns_them() {
        let env = setup("pt-datas");
        let r = exec(
            "datas_add",
            json!({ "title": "FTP example.ru", "body": "login: user-1\npass: s3cret" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["updated"], false);

        let r = exec("datas_read", json!({}), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(content.contains("## FTP example.ru"));
        assert!(content.contains("user-1"));

        // same title updates in place instead of duplicating
        let r = exec(
            "datas_add",
            json!({ "title": "FTP example.ru", "body": "login: user-2" }),
            &env.ctx,
        );
        assert_eq!(r.data.unwrap()["updated"], true);
        let r = exec("datas_read", json!({}), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert_eq!(content.matches("## FTP example.ru").count(), 1);
        assert!(content.contains("user-2") && !content.contains("user-1"));

        // a second title appends a second section
        exec(
            "datas_add",
            json!({ "title": "Admin panel", "body": "https://admin.example.ru" }),
            &env.ctx,
        );
        let r = exec("datas_read", json!({}), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(content.contains("## FTP example.ru") && content.contains("## Admin panel"));
        assert_eq!(content.matches("## ").count(), 2);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn policies_and_classes_match_the_spec() {
        for name in [
            "search",
            "read_file",
            "list_tree",
            "ocr",
            "task_list",
            "datas_read",
            "git_status",
            "git_diff",
            "git_log",
            "webfetch",
            "websearch",
            "skill",
            "recall",
            "todoread",
        ] {
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
        }
        for name in [
            "write_file",
            "edit_file",
            "create_folder",
            "task_create",
            "task_update",
            "task_move",
            "datas_add",
            "create_skill",
            "remember",
            "set_user_gender",
            "todowrite",
        ] {
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
        }
        for name in ["rename_path", "delete_path", "task_delete", "git_commit", "git_pull", "git_push", "apply_patch"] {
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
        }
        assert_eq!(tool("git_branch").class(), ToolClass::Mutating);
        assert_eq!(tool("git_branch").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("image_transform").class(), ToolClass::Mutating);
        assert_eq!(tool("image_transform").policy(), PermissionPolicy::CheckConditional);
        let env = setup("pt-image-policy");
        let img = tool("image_transform");
        assert!(img
            .danger_reason(&json!({ "path": "a.png", "maxSize": 10 }), &env.ctx)
            .is_some());
        assert!(img
            .danger_reason(&json!({ "path": "a.png", "output": "b.png", "maxSize": 10 }), &env.ctx)
            .is_none());
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
