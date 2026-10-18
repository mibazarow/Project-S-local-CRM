// Harness tool set (SPEC 5.2 items 1, 2, 4, 5): machine-wide fs tools, a bash
// tool, CRM tools, and the scheduler tools. Unlike the project tools, harness
// paths are absolute, ~-expanded, or relative to the working dir; the default
// territory is Harness/. fs_write skips confirmation only inside Harness/ and
// Projects/ (CheckConditional), and so do fs_copy/fs_move unless overwrite is
// requested or an endpoint lies outside, and fs_rename/fs_mkdir when the path
// lies outside; fs_delete and bash are always
// confirmed — the
// confirmation is the guard, there is no path allowlist. Child processes of
// bash always see TMPDIR/TEMP/TMP pointed at Harness/tmp/ (SPEC 5.2), never
// the system /tmp. The CDP browser tool lives in browser_tool.rs.
use super::catalog::Catalog;
use crate::sync_util::lock_recover;
use super::project_tools::{
    emit_tasks_changed, emit_trash_changed, emit_versions_changed, envelope, require_wd,
    state_of, with_conn,
};
use super::task_glue::{
    create_schema, datas_add_body, datas_title_arg, delete_schema, list_schema, move_schema,
    parse_checklist_arg, parse_task_patch, read_datas, task_create_body, task_delete_body,
    task_move_body, task_owner, task_update_body, tasks_list_filtered, update_schema,
};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::scheduler::{self, CreateIn};
use crate::{db, embed, extract, fs_ops, paths, projects, search, tasks, undo};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GREP_SCANNED_FILES: usize = 10_000;
const GREP_SKIP_DIRS: [&str; 3] = ["node_modules", "target", ".git"];
const LIST_MAX_ENTRIES: usize = 500;
const BASH_TIMEOUT_SECS: u64 = 120;
const BASH_OUTPUT_CAP: usize = 30_000;

// Shared by fs_read and the project read_file: images are not text and must
// not be base64-inlined into the tool result (SPEC: attach in chat instead).
pub(crate) const IMAGE_READ_HINT: &str = "Image files cannot be read as text; attach the image in the chat so a vision-capable model can see it.";

// Envelope headroom for extracted documents: the JSON wrapper around the
// content (keys, path, escaping) costs a little, so the extracted text is
// capped below max_result_len to keep the capped envelope inside the limit.
const EXTRACT_ENVELOPE_RESERVE: usize = 512;

// Core (finish/skill/memory) stays active from the first round; the
// machine-wide set is registered for lazy discovery (SPEC 6.4) so the
// harness widens its own schema list with enable_tools.
pub fn harness_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let core: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::agent::finish::FinishTaskTool),
        Arc::new(crate::skills::SkillTool),
        Arc::new(crate::skills::SkillExecTool),
        Arc::new(crate::memory::RememberTool),
        Arc::new(crate::memory::RecallTool),
        Arc::new(crate::memory::SetUserGenderTool),
        Arc::new(crate::memory::ForgetTool),
        Arc::new(ListChatsTool),
        Arc::new(RenameChatTool),
        Arc::new(NewChatTool),
        Arc::new(DeleteChatTool),
        Arc::new(SearchChatsTool),
        // Per-conversation plan panel (SPEC: todowrite/todoread).
        Arc::new(crate::todos::TodoWriteTool),
        Arc::new(crate::todos::TodoReadTool),
        // Long-lived goals with steps (objectives slice): global goals the
        // user sees with progress in the Today panel.
        Arc::new(crate::objectives::ObjectiveCreateTool),
        Arc::new(crate::objectives::ObjectiveListTool),
        Arc::new(crate::objectives::ObjectiveUpdateTool),
        Arc::new(crate::objectives::ObjectiveStepDoneTool),
        // Agent-initiated chat attachment (SPEC 6.1): hand a file to the user
        // as a new assistant turn; active from the first round.
        Arc::new(super::send_file::SendFileTool),
    ];
    let deferred: Vec<Arc<dyn Tool>> = vec![
        Arc::new(FsReadTool),
        Arc::new(FsListTool),
        Arc::new(FsWriteTool),
        Arc::new(FsDeleteTool),
        Arc::new(FsCopyTool),
        Arc::new(FsMoveTool),
        Arc::new(FsRenameTool),
        Arc::new(FsMkdirTool),
        Arc::new(FsGrepTool),
        // Precise edits (exact string replacement + git apply patches);
        // see edit_tools.rs for the safety rules.
        Arc::new(super::edit_tools::EditFileTool::harness()),
        Arc::new(super::edit_tools::ApplyPatchTool),
        // Native document generation (DOCX/PDF/MD/TXT/HTML + XLSX/CSV) with
        // pure Rust writers; see doc_tools.rs for the safety rules.
        Arc::new(super::doc_tools::CreateDocumentTool::harness()),
        Arc::new(super::doc_tools::CreateSpreadsheetTool::harness()),
        // OCR of images and PDFs via the macOS Vision framework through an
        // embedded Swift helper; see ocr_tools.rs for the safety rules.
        Arc::new(super::ocr_tools::OcrTool),
        // PDF merge/split/fill-form via Apple PDFKit through an embedded Swift
        // helper; see pdf_tools.rs for the safety rules.
        Arc::new(super::pdf_tools::PdfMergeTool),
        Arc::new(super::pdf_tools::PdfSplitTool),
        Arc::new(super::pdf_tools::PdfFillFormTool),
        // Image resize/crop/rotate/convert via the macOS sips CLI; in-place
        // calls confirm, new files in the territory skip; see image_tools.rs.
        Arc::new(super::image_tools::ImageTransformTool),
        // Screen capture via the macOS screencapture CLI; always confirmed,
        // see screenshot_tool.rs for the safety rules.
        Arc::new(super::screenshot_tool::ScreenshotTool::default()),
        Arc::new(crate::skills::CreateSkillTool),
        Arc::new(BashTool::default()),
        // Background process control (dev servers, installs): the background
        // twin of bash, lazy-discovered together with it; see process_tools.rs.
        Arc::new(super::process_tools::ProcessStartTool),
        Arc::new(super::process_tools::ProcessLogsTool),
        Arc::new(super::process_tools::ProcessStopTool),
        Arc::new(super::process_tools::ProcessListTool),
        Arc::new(super::browser_tool::BrowserTool),
        Arc::new(super::net_tools::WebfetchTool::new()),
        Arc::new(super::net_tools::WebsearchTool::new()),
        Arc::new(super::net_tools::HttpRequestTool),
        Arc::new(CrmListProjectsTool),
        Arc::new(CrmSearchTool),
        Arc::new(CrmTasksTool),
        Arc::new(CrmTaskCreateTool),
        Arc::new(CrmTaskUpdateTool),
        Arc::new(CrmTaskMoveTool),
        Arc::new(CrmTaskDeleteTool),
        Arc::new(DatasReadTool),
        Arc::new(DatasAddTool),
        // Git tools via the system git CLI, lazy-discovered like the rest of
        // the machine-wide set; see git_tools.rs for the safety rules.
        Arc::new(super::git_tools::GitStatusTool),
        Arc::new(super::git_tools::GitDiffTool),
        Arc::new(super::git_tools::GitLogTool),
        Arc::new(super::git_tools::GitBranchTool),
        Arc::new(super::git_tools::GitCommitTool),
        Arc::new(super::git_tools::GitPullTool),
        Arc::new(super::git_tools::GitPushTool),
        // SSH tools via the system ssh/scp/ssh-keygen CLI: keys, ssh-agent
        // and askpass passwords, profiles from ssh.json and the current
        // project's datas.md; lazy discovered like the rest of the
        // machine-wide set; see ssh_tools.rs.
        Arc::new(super::ssh_tools::SshListProfilesTool),
        Arc::new(super::ssh_tools::SshExecTool),
        Arc::new(super::ssh_tools::SshCopyTool::upload()),
        Arc::new(super::ssh_tools::SshCopyTool::download()),
        Arc::new(super::ssh_tools::SshGenerateKeyTool),
        Arc::new(super::ssh_tools::SshInstallKeyTool),
        Arc::new(super::ssh_tools::SshSaveProfileToDatasTool),
        // macOS clipboard read/write via the system pbcopy/pbpaste, lazy
        // discovered like the rest of the machine-wide set; see clipboard_tools.rs.
        Arc::new(super::clipboard_tools::ClipboardGetTool::default()),
        Arc::new(super::clipboard_tools::ClipboardSetTool::default()),
        Arc::new(ScheduleCreateTool),
        Arc::new(ScheduleListTool),
        Arc::new(SchedulePauseTool),
        Arc::new(ScheduleDeleteTool),
        Arc::new(crate::agent::undo_tools::UndoLastTool),
        Arc::new(crate::agent::undo_tools::UndoListTool),
        // Background entity links (links slice): lazy-discovered like the
        // rest of the machine-wide set.
        Arc::new(crate::links::RelatedTool),
    ];
    for tool in core {
        catalog.register(tool, true).expect("unique tool names");
    }
    for tool in deferred {
        catalog.register(tool, false).expect("unique tool names");
    }
    catalog
}

// Expands "~", joins relative paths onto the working dir, then collapses
// "." and ".." lexically (the target may not exist yet, e.g. fs_write).
pub(crate) fn resolve_path(raw: &str, wd: &Path) -> PathBuf {
    let expanded = if raw == "~" {
        home_dir()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir().join(rest)
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        wd.join(raw)
    };
    normalize(&expanded)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// Best-effort canonical form for policy checks: existing prefixes resolve
// through symlinks, the missing tail (fs_write targets) is appended back.
pub(crate) fn canonicalish(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    loop {
        match cur.canonicalize() {
            Ok(c) => {
                let mut out = c;
                while let Some(name) = tail.pop() {
                    out.push(name);
                }
                return out;
            }
            Err(_) => match (cur.parent(), cur.file_name()) {
                (Some(p), Some(name)) => {
                    tail.push(name.to_os_string());
                    cur = p;
                }
                _ => return normalize(path),
            },
        }
    }
}

// SPEC 5.2: writes inside Harness/ and Projects/ skip confirmation.
pub(crate) fn in_default_territory(path: &Path, wd: &Path) -> bool {
    let base = wd.canonicalize().unwrap_or_else(|_| normalize(wd));
    let resolved = canonicalish(path);
    resolved.starts_with(base.join(paths::HARNESS_DIR))
        || resolved.starts_with(base.join(paths::PROJECTS_DIR))
}

// Minimal sanity guard for fs_delete; the confirmation remains the real one.
fn is_obviously_destructive(path: &Path, wd: &Path) -> bool {
    let resolved = canonicalish(path);
    let base = wd.canonicalize().unwrap_or_else(|_| normalize(wd));
    resolved == Path::new("/")
        || resolved == normalize(&home_dir())
        || resolved == base
        || resolved == base.join(paths::HARNESS_DIR)
        || resolved == base.join(paths::PROJECTS_DIR)
}

fn io_fail(path: &Path, e: &std::io::Error) -> ToolResult {
    if e.kind() == std::io::ErrorKind::NotFound {
        ToolResult::err(ToolErrorCode::NotFound, format!("{} not found", path.display()))
    } else {
        ToolResult::err(ToolErrorCode::ExecutionError, format!("{}: {e}", path.display()))
    }
}

// Lenient re-anchoring for paths that must already exist: an exact hit
// passes through untouched; a miss is re-resolved component by component
// against the real folder entries (NFC/NFD, case, dash and space variants),
// returning the ACTUAL on-disk path. Ambiguous matches and misses surface
// as AppError (see lenient_fail).
pub(crate) fn lenient_existing(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(path.to_path_buf());
    }
    fs_ops::lenient_absolute(path)
}

// Maps a lenient-resolution failure to the envelope: ambiguous candidates
// are invalid_args (the caller must pick one), plain misses keep not_found.
pub(crate) fn lenient_fail(e: &AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if msg.contains("ambiguous") {
        ToolErrorCode::InvalidArgs
    } else {
        ToolErrorCode::NotFound
    };
    ToolResult::err(code, msg)
}

// Resolves the parent folders of a create/write target leniently, keeping
// the final component as requested; a genuinely missing parent chain is
// returned unchanged so the caller creates it.
fn lenient_target_parent(path: &Path) -> Result<PathBuf> {
    fs_ops::lenient_parent(path)
}

// Harness CRM tools address projects by id or exact name. Cross-project
// search iterates all projects: the UI keeps search per-project (SPEC 4.4),
// but the harness agent explicitly gets CRM-wide tools (SPEC 5.2 #4).
fn resolve_project_id(conn: &Connection, args: &Value) -> Result<i64> {
    if let Some(id) = args.get("project_id").and_then(Value::as_i64) {
        let exists: i64 =
            conn.query_row("SELECT count(*) FROM projects WHERE id = ?1", params![id], |r| r.get(0))?;
        if exists == 0 {
            return Err(AppError::Invalid(format!("project {id} not found")));
        }
        return Ok(id);
    }
    if let Some(name) = args.get("project_name").and_then(Value::as_str) {
        return conn
            .query_row(
                "SELECT id FROM projects WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| AppError::Invalid(format!("project \"{name}\" not found")));
    }
    Err(AppError::Invalid("project_id or project_name is required".into()))
}

pub struct FsReadTool;

impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a file anywhere on the machine (absolute, ~/ or working-dir-relative path). Text files are returned as-is; docx/xlsx/xlsm/pdf are extracted to text (extracted: true). Images and other binary or >2 MiB files are rejected."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
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
        let wd = require_wd(ctx)?;
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) => return Ok(lenient_fail(&e)),
        };
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        if meta.is_dir() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, format!("{} is a directory", path.display())));
        }
        if meta.len() > MAX_READ_BYTES {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("{} is {} bytes, over the {}-byte read cap; use fs_grep or a narrower read", path.display(), meta.len(), MAX_READ_BYTES),
            ));
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if fs_ops::IMAGE_EXTS.contains(&ext.as_str()) {
            return Ok(ToolResult::err(ToolErrorCode::ExecutionError, IMAGE_READ_HINT));
        }
        if matches!(extract::kind(&path), Some("docx") | Some("xlsx") | Some("pdf")) {
            let max_chars = self.max_result_len() - EXTRACT_ENVELOPE_RESERVE;
            return match extract::extract_text(&path, max_chars) {
                Ok(content) => Ok(ToolResult::ok(json!({
                    "path": path.display().to_string(),
                    "content": content,
                    "extracted": true,
                    "format": ext,
                }))
                .capped(self.max_result_len())),
                Err(e) => Ok(envelope_r(e)),
            };
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        if bytes.contains(&0) {
            return Ok(ToolResult::err(ToolErrorCode::ExecutionError, format!("{} looks binary", path.display())));
        }
        Ok(ToolResult::ok(json!({
            "path": path.display().to_string(),
            "content": String::from_utf8_lossy(&bytes),
        }))
        .capped(self.max_result_len()))
    }
}

pub struct FsListTool;

impl Tool for FsListTool {
    fn name(&self) -> &str {
        "fs_list"
    }
    fn description(&self) -> &str {
        "List a folder anywhere on the machine (default: the Harness/ territory): name, kind, size per entry."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
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
        let wd = require_wd(ctx)?;
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => match lenient_existing(&resolve_path(p, &wd)) {
                Ok(p) => p,
                Err(e) => return Ok(lenient_fail(&e)),
            },
            _ => paths::harness_dir(&wd),
        };
        let rd = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        let mut entries: Vec<Value> = rd
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some(json!({
                    "name": e.file_name().to_string_lossy(),
                    "kind": if meta.is_dir() { "dir" } else { "file" },
                    "size": if meta.is_file() { serde_json::json!(meta.len()) } else { Value::Null },
                }))
            })
            .collect();
        entries.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
        let truncated = entries.len() > LIST_MAX_ENTRIES;
        entries.truncate(LIST_MAX_ENTRIES);
        Ok(ToolResult::ok(json!({ "path": path.display().to_string(), "entries": entries, "truncated": truncated }))
            .capped(self.max_result_len()))
    }
}

pub struct FsWriteTool;

impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file anywhere on the machine; parent folders are created. Writes inside Harness/ and Projects/ run without confirmation, elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let wd = ctx.working_dir.as_deref()?;
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = resolve_path(raw, wd);
        (!in_default_territory(&path, wd)).then(|| "запись вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let content = args["content"].as_str().unwrap_or_default();
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        // An existing file (also via a lenient variant match) is overwritten;
        // a new one keeps the requested name under leniently resolved parents.
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) if e.to_string().contains("ambiguous") => return Ok(lenient_fail(&e)),
            Err(_) => lenient_target_parent(&path)?,
        };
        if let Ok(meta) = fs::metadata(&path) {
            if meta.is_dir() {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, format!("{} is a directory", path.display())));
            }
        }
        let existed = path.exists();
        let previous = if existed {
            fs::read_to_string(&path).ok()
        } else {
            None
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return Ok(io_fail(parent, &e));
            }
        }
        match fs::write(&path, content) {
            Ok(()) => {
                let abs = path.display().to_string();
                if let Some(prev) = previous {
                    let version_id = with_conn(ctx, |conn| {
                        crate::reliability::version_save(conn, &wd, "harness", &abs, &prev)
                    })
                    .ok();
                    if version_id.is_some() {
                        emit_versions_changed(ctx, "harness", &abs);
                        let _ = with_conn(ctx, |conn| {
                            crate::undo::push_file_write(
                                conn,
                                "harness",
                                &abs,
                                version_id,
                                false,
                                None,
                                None,
                            )
                        });
                    }
                } else if !existed {
                    let _ = with_conn(ctx, |conn| {
                        crate::undo::push_file_write(
                            conn,
                            "harness",
                            &abs,
                            None,
                            true,
                            None,
                            None,
                        )
                    });
                }
                Ok(ToolResult::ok(json!({
                    "path": path.display().to_string(),
                    "bytesWritten": content.len(),
                })))
            }
            Err(e) => Ok(io_fail(&path, &e)),
        }
    }
}

pub struct FsDeleteTool;

impl Tool for FsDeleteTool {
    fn name(&self) -> &str {
        "fs_delete"
    }
    fn description(&self) -> &str {
        "Delete a file or folder (recursively) anywhere on the machine. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление файла или папки".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) => return Ok(lenient_fail(&e)),
        };
        if is_obviously_destructive(&path, &wd) {
            return Ok(ToolResult::err(
                ToolErrorCode::PermissionDenied,
                format!("refusing to delete a root folder: {}", path.display()),
            ));
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        // Reversible delete: the item moves into Settings/.trash and can be
        // restored through the trash commands.
        let kind = if meta.is_dir() { "dir" } else { "file" };
        let r = with_conn(ctx, |conn| {
            crate::reliability::trash_put(conn, &wd, "harness", kind, &path, None)
        });
        match r {
            Ok(id) => {
                let _ = with_conn(ctx, |conn| {
                    crate::undo::push_delete(conn, id, "harness", &path.display().to_string(), None, None)
                });
                emit_trash_changed(ctx, Some(id));
                Ok(ToolResult::ok(json!({
                    "deleted": path.display().to_string(),
                    "trashId": id,
                })))
            }
            Err(e) => Ok(envelope_r(e)),
        }
    }
}

// EXDEV (cross-device link): errno 18 on both macOS and Linux; rename()
// fails with it when from/to live on different volumes, and fs_move then
// falls back to copy + remove.
const EXDEV: i32 = 18;

// First symlink component on the way to `path` below the working dir (the
// final component included), so auto-created parents and in-territory links
// can never land outside the territory the policy checked.
fn symlink_below_wd(path: &Path, wd: &Path) -> Option<PathBuf> {
    let Ok(rest) = path.strip_prefix(wd) else {
        return None;
    };
    let mut cur = wd.to_path_buf();
    for part in rest.components() {
        cur.push(part.as_os_str());
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Some(cur);
        }
    }
    None
}

// Symlink anywhere under the working dir, or the final entry of a path
// outside it: fs_copy/fs_move never follow or replace symlinks.
fn symlink_guard(path: &Path, wd: &Path) -> Option<PathBuf> {
    symlink_below_wd(path, wd).or_else(|| {
        fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.file_type().is_symlink())
            .map(|_| path.to_path_buf())
    })
}

// Recursive copy; symlinks inside the tree are skipped, never followed.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<u64> {
    fs::create_dir_all(to)?;
    let mut bytes = 0u64;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for item in fs::read_dir(&src_dir)? {
            let item = item?;
            let ft = item.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let dst = dst_dir.join(item.file_name());
            if ft.is_dir() {
                fs::create_dir_all(&dst)?;
                stack.push((item.path(), dst));
            } else {
                bytes += fs::copy(item.path(), &dst)?;
            }
        }
    }
    Ok(bytes)
}

// Total size of a file or tree (symlinks not followed), for the bytes field.
fn entry_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for item in rd.flatten() {
            if let Ok(ft) = item.file_type() {
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    stack.push(item.path());
                } else if let Ok(m) = item.metadata() {
                    total += m.len();
                }
            }
        }
    }
    total
}

// CheckConditional: confirmation is skipped only when both endpoints are
// inside Harness/ and Projects/ and nothing is replaced; overwrite always
// confirms, even inside the territory.
fn transfer_danger_reason(args: &Value, ctx: &ToolCtx, verb: &str) -> Option<String> {
    let wd = ctx.working_dir.as_deref()?;
    let from = resolve_path(args.get("from").and_then(Value::as_str)?, wd);
    let to = resolve_path(args.get("to").and_then(Value::as_str)?, wd);
    let mut reasons: Vec<String> = Vec::new();
    if !(in_default_territory(&from, wd) && in_default_territory(&to, wd)) {
        reasons.push(format!("{verb} вне Harness/ и Projects/"));
    }
    if args.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
        reasons.push("overwrite=true перезапишет существующий файл".to_string());
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

// Shared fs_copy/fs_move body: resolve both paths, refuse symlinks and
// directory-into-itself, apply the collision rules, then transfer.
fn fs_transfer(args: &Value, ctx: &ToolCtx, move_it: bool, max_len: usize) -> Result<ToolResult> {
    let wd = require_wd(ctx)?;
    let verb = if move_it { "move" } else { "copy" };
    let from_raw = args["from"].as_str().unwrap_or_default();
    let to_raw = args["to"].as_str().unwrap_or_default();
    if from_raw.is_empty() || to_raw.is_empty() {
        return Ok(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            "from and to are required",
        ));
    }
    let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    let from = resolve_path(from_raw, &wd);
    let to = resolve_path(to_raw, &wd);
    let from = match lenient_existing(&from) {
        Ok(p) => p,
        Err(e) => return Ok(lenient_fail(&e)),
    };
    // An existing destination (also via a lenient variant match) keeps its
    // real name for the collision rules; a genuinely new one keeps the
    // requested name. An ambiguous destination is refused, never guessed.
    let to = match lenient_existing(&to) {
        Ok(p) => p,
        Err(e) if e.to_string().contains("ambiguous") => return Ok(lenient_fail(&e)),
        Err(_) => to,
    };
    for path in [&from, &to] {
        if let Some(link) = symlink_guard(path, &wd) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("{} is a symlink; {verb} does not follow symlinks", link.display()),
            ));
        }
    }
    let meta = match fs::symlink_metadata(&from) {
        Ok(m) => m,
        Err(e) => return Ok(io_fail(&from, &e)),
    };
    let is_dir = meta.is_dir();
    // An existing directory `to` means "transfer into it", keeping the name.
    let target = if fs::metadata(&to).map(|m| m.is_dir()).unwrap_or(false) {
        match from.file_name() {
            Some(name) => to.join(name),
            None => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} has no name to {verb}", from.display()),
                ))
            }
        }
    } else {
        to.clone()
    };
    if let Some(link) = symlink_guard(&target, &wd) {
        return Ok(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!("{} is a symlink; {verb} does not follow symlinks", link.display()),
        ));
    }
    if is_dir && canonicalish(&target).starts_with(canonicalish(&from)) {
        return Ok(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!(
                "cannot {verb} a directory into itself or its own subfolder: {}",
                target.display()
            ),
        ));
    }
    if let Ok(tmeta) = fs::symlink_metadata(&target) {
        if !overwrite {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "destination exists: {}; pass overwrite=true to replace a file",
                    target.display()
                ),
            ));
        }
        if is_dir || tmeta.is_dir() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "overwrite only replaces files, not directories; remove {} with fs_delete first",
                    target.display()
                ),
            ));
        }
    }
    let target_existed = fs::symlink_metadata(&target).is_ok();
    let target_previous = if target_existed && !is_dir {
        fs::read_to_string(&target).ok()
    } else {
        None
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return Ok(io_fail(parent, &e));
        }
    }
    let bytes = if move_it {
        let total = entry_bytes(&from);
        match fs::rename(&from, &target) {
            Ok(()) => total,
            Err(e) if e.raw_os_error() == Some(EXDEV) => {
                let copied = if is_dir {
                    copy_tree(&from, &target)
                } else {
                    fs::copy(&from, &target)
                };
                match copied {
                    Ok(n) => {
                        let removed = if is_dir {
                            fs::remove_dir_all(&from)
                        } else {
                            fs::remove_file(&from)
                        };
                        if let Err(e) = removed {
                            return Ok(ToolResult::err(
                                ToolErrorCode::ExecutionError,
                                format!(
                                    "copied {} to {} but could not remove the source: {e}",
                                    from.display(),
                                    target.display()
                                ),
                            ));
                        }
                        n
                    }
                    Err(e) => return Ok(io_fail(&target, &e)),
                }
            }
            Err(e) => return Ok(io_fail(&from, &e)),
        }
    } else if is_dir {
        match copy_tree(&from, &target) {
            Ok(n) => n,
            Err(e) => return Ok(io_fail(&target, &e)),
        }
    } else {
        match fs::copy(&from, &target) {
            Ok(n) => n,
            Err(e) => return Ok(io_fail(&target, &e)),
        }
    };
    if move_it {
        let _ = with_conn(ctx, |conn| {
            undo::push_rename(
                conn,
                "harness",
                &from.display().to_string(),
                &target.display().to_string(),
                None,
                None,
            )
        });
    } else if let Some(prev) = target_previous {
        let abs = target.display().to_string();
        let version_id = with_conn(ctx, |conn| {
            crate::reliability::version_save(conn, &wd, "harness", &abs, &prev)
        })
        .ok();
        if version_id.is_some() {
            emit_versions_changed(ctx, "harness", &abs);
            let _ = with_conn(ctx, |conn| {
                undo::push_file_write(conn, "harness", &abs, version_id, false, None, None)
            });
        }
    } else if !target_existed {
        let _ = with_conn(ctx, |conn| {
            undo::push_file_write(
                conn,
                "harness",
                &target.display().to_string(),
                None,
                true,
                None,
                None,
            )
        });
    }
    Ok(ToolResult::ok(json!({
        "from": from.display().to_string(),
        "to": target.display().to_string(),
        "kind": if is_dir { "dir" } else { "file" },
        "bytes": bytes,
    }))
    .capped(max_len))
}

pub struct FsCopyTool;

impl Tool for FsCopyTool {
    fn name(&self) -> &str {
        "fs_copy"
    }
    fn description(&self) -> &str {
        "Copy a file or folder (recursively) anywhere on the machine (absolute, ~/ or working-dir-relative paths). Parent folders of the target are created; to may be a new path or an existing folder (the source is placed into it by name). Symlinks are never followed or copied, and a folder is never copied into itself. The destination is only replaced with overwrite=true, which replaces files but never merges or replaces folders. Copies inside Harness/ and Projects/ run without confirmation unless overwrite=true; elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string" },
                "to": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        transfer_danger_reason(args, ctx, "копирование")
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        fs_transfer(args, ctx, false, self.max_result_len())
    }
}

pub struct FsMoveTool;

impl Tool for FsMoveTool {
    fn name(&self) -> &str {
        "fs_move"
    }
    fn description(&self) -> &str {
        "Move a file or folder anywhere on the machine (rename; falls back to copy+delete across volumes). to may be a new path or an existing folder (the source is moved into it by name); parent folders are created. Symlinks are never followed, and a folder is never moved into itself. The destination is only replaced with overwrite=true, which replaces files but never folders. Moves inside Harness/ and Projects/ run without confirmation unless overwrite=true; elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string" },
                "to": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        transfer_danger_reason(args, ctx, "перемещение")
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        fs_transfer(args, ctx, true, self.max_result_len())
    }
}

pub struct FsRenameTool;

impl Tool for FsRenameTool {
    fn name(&self) -> &str {
        "fs_rename"
    }
    fn description(&self) -> &str {
        "Rename a file or folder inside its current parent folder: path plus newName (a plain name without separators or traversal). The destination must not exist. Renames inside Harness/ and Projects/ run without confirmation, elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "newName": { "type": "string" }
            },
            "required": ["path", "newName"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let wd = ctx.working_dir.as_deref()?;
        let path = resolve_path(args.get("path").and_then(Value::as_str)?, wd);
        (!in_default_territory(&path, wd)).then(|| "переименование вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) => return Ok(lenient_fail(&e)),
        };
        let new_name = match fs_ops::check_name(args["newName"].as_str().unwrap_or_default()) {
            Ok(name) => name,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string())),
        };
        let parent = match path.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} has no parent folder to rename in", path.display()),
                ))
            }
        };
        let target = parent.join(&new_name);
        if target != path && fs::symlink_metadata(&target).is_ok() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("destination exists: {}", target.display()),
            ));
        }
        match fs::rename(&path, &target) {
            Ok(()) => {
                let _ = with_conn(ctx, |conn| {
                    undo::push_rename(
                        conn,
                        "harness",
                        &path.display().to_string(),
                        &target.display().to_string(),
                        None,
                        None,
                    )
                });
                Ok(ToolResult::ok(json!({
                    "from": path.display().to_string(),
                    "to": target.display().to_string(),
                })))
            }
            Err(e) => Ok(io_fail(&path, &e)),
        }
    }
}

pub struct FsMkdirTool;

impl Tool for FsMkdirTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Create a single folder: the parent must already exist (parents are not created) and the path must not exist yet. Creation inside Harness/ and Projects/ runs without confirmation, elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let wd = ctx.working_dir.as_deref()?;
        let path = resolve_path(args.get("path").and_then(Value::as_str)?, wd);
        (!in_default_territory(&path, wd)).then(|| "создание папки вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let raw = args["path"].as_str().unwrap_or_default();
        if raw.trim().is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "path is empty"));
        }
        let path = resolve_path(raw, &wd);
        // A variant of an existing folder name reports the real one instead
        // of creating a near-duplicate folder next to it.
        let path = match lenient_existing(&path) {
            Ok(existing) => {
                let kind = if fs::symlink_metadata(&existing)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                {
                    "folder"
                } else {
                    "file"
                };
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} already exists as a {kind}", existing.display()),
                ));
            }
            Err(e) if e.to_string().contains("ambiguous") => return Ok(lenient_fail(&e)),
            Err(_) => lenient_target_parent(&path)?,
        };
        let Some(parent) = path.parent() else {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("{} has no parent folder", path.display()),
            ));
        };
        match fs::create_dir(&path) {
            Ok(()) => Ok(ToolResult::ok(json!({
                "path": path.display().to_string(),
                "created": true,
            }))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("parent folder does not exist: {}", parent.display()),
            )),
            Err(e) => Ok(io_fail(&path, &e)),
        }
    }
}

pub struct FsGrepTool;

fn grep_file(path: &Path, lower: &str, matches: &mut Vec<Value>) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return;
    };
    for (no, line) in text.lines().enumerate() {
        if line.to_lowercase().contains(lower) {
            let text: String = line.chars().take(200).collect();
            matches.push(json!({
                "path": path.display().to_string(),
                "line": no + 1,
                "text": text,
            }));
        }
    }
}

impl Tool for FsGrepTool {
    fn name(&self) -> &str {
        "fs_grep"
    }
    fn description(&self) -> &str {
        "Recursive case-insensitive text search under a folder (default: Harness/). Skips binary, >1 MiB files and .git/node_modules/target; returns path, line, text per match."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "max_results": { "type": "number" }
            },
            "required": ["pattern"],
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
        let wd = require_wd(ctx)?;
        let pattern = args["pattern"].as_str().unwrap_or_default();
        if pattern.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "pattern is empty"));
        }
        let max = args
            .get("max_results")
            .and_then(Value::as_f64)
            .unwrap_or(50.0)
            .clamp(1.0, 200.0) as usize;
        let root = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => match lenient_existing(&resolve_path(p, &wd)) {
                Ok(p) => p,
                Err(e) => return Ok(lenient_fail(&e)),
            },
            _ => paths::harness_dir(&wd),
        };
        if let Err(e) = fs::metadata(&root) {
            return Ok(io_fail(&root, &e));
        }
        let lower = pattern.to_lowercase();
        let mut matches: Vec<Value> = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        let mut stack = vec![root.clone()];
        'walk: while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if meta.is_dir() {
                    if name.starts_with('.') || GREP_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(entry.path());
                } else {
                    scanned += 1;
                    if scanned > MAX_GREP_SCANNED_FILES {
                        truncated = true;
                        break 'walk;
                    }
                    if meta.len() > MAX_GREP_FILE_BYTES {
                        continue;
                    }
                    grep_file(&entry.path(), &lower, &mut matches);
                    if matches.len() >= max {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
        }
        matches.truncate(max);
        Ok(ToolResult::ok(json!({
            "pattern": pattern,
            "root": root.display().to_string(),
            "matches": matches,
            "truncated": truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct BashTool {
    timeout: Duration,
}

impl Default for BashTool {
    fn default() -> Self {
        BashTool {
            timeout: Duration::from_secs(BASH_TIMEOUT_SECS),
        }
    }
}

impl BashTool {
    pub fn with_timeout(timeout: Duration) -> Self {
        BashTool { timeout }
    }
}

// Reads a pipe up to the cap plus one byte so truncation is detectable even
// if the command keeps producing (then the pipe fills and the timeout kills
// the command — the output is capped anyway).
fn read_capped(mut reader: impl Read) -> (String, bool) {
    let mut buf = Vec::new();
    let _ = (&mut reader)
        .take((BASH_OUTPUT_CAP as u64) + 1)
        .read_to_end(&mut buf);
    let truncated = buf.len() > BASH_OUTPUT_CAP;
    buf.truncate(BASH_OUTPUT_CAP);
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command via sh -c (default cwd: Harness/). TMPDIR/TEMP/TMP point at Harness/tmp/, never the system /tmp. Returns exitCode, stdout, stderr; 120 s timeout."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "cwd": { "type": "string" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("выполнение произвольной shell-команды".to_string())
    }
    fn max_result_len(&self) -> usize {
        80_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let command = args["command"].as_str().unwrap_or_default();
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp)?;
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(c) if !c.is_empty() => resolve_path(c, &wd),
            _ => paths::harness_dir(&wd),
        };
        fs::create_dir_all(&cwd)?;
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || stdout.map(read_capped));
        let err_thread = std::thread::spawn(move || stderr.map(read_capped));
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("command timed out after {} s and was killed", self.timeout.as_secs_f64()),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let (stdout, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
        let (stderr, err_cut) = err_thread.join().unwrap_or_default().unwrap_or_default();
        Ok(ToolResult::ok(json!({
            "cwd": cwd.display().to_string(),
            "exitCode": status.code().unwrap_or(-1),
            "stdout": stdout,
            "stderr": stderr,
            "truncated": out_cut || err_cut,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct CrmListProjectsTool;

impl Tool for CrmListProjectsTool {
    fn name(&self) -> &str {
        "crm_list_projects"
    }
    fn description(&self) -> &str {
        "List all CRM projects with id, name, path and counts (files, tasks, overdue)."
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
        let r = with_conn(ctx, projects::list_projects);
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "projects": list })).capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

// CRM domain errors map through the same envelope as the project tools.
fn envelope_r(e: AppError) -> ToolResult {
    envelope::<()>(Err(e))
}

pub struct CrmSearchTool;

impl Tool for CrmSearchTool {
    fn name(&self) -> &str {
        "crm_search"
    }
    fn description(&self) -> &str {
        "Hybrid index search (FTS + semantic). With project_id/project_name: one project; without: across all projects (harness-only capability)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "project_id": { "type": "number" },
                "project_name": { "type": "string" },
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
        let state = state_of(ctx)?;
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as usize;
        let scoped = args.get("project_id").is_some() || args.get("project_name").is_some();
        // S1.2: one connection serves the whole call (project resolution plus
        // every project's search legs), not one per project.
        let wd = state.working_dir()?;
        let conn = db::open(&paths::db_path(&wd))?;
        let embedder = embed::select_embedder(&wd);
        let pids: Vec<i64> = if scoped {
            vec![resolve_project_id(&conn, args)?]
        } else {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut hits: Vec<Value> = Vec::new();
        for pid in pids {
            for hit in search::search_with(&conn, embedder.as_ref(), embed::NAME_WEIGHT, pid, query, limit)? {
                let mut v = serde_json::to_value(&hit).unwrap_or(Value::Null);
                v["projectId"] = json!(pid);
                hits.push(v);
            }
        }
        hits.sort_by(|a, b| {
            b["score"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len()))
    }
}

pub struct CrmTasksTool;

impl Tool for CrmTasksTool {
    fn name(&self) -> &str {
        "crm_tasks"
    }
    fn description(&self) -> &str {
        "List a project's tasks (project_id or project_name); optional status filter (new|progress|done)."
    }
    fn parameters(&self) -> Value {
        list_schema(true)
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let status = args.get("status").and_then(Value::as_str).map(str::to_owned);
        let r = with_conn(ctx, |conn| {
            let pid = resolve_project_id(conn, args)?;
            tasks_list_filtered(conn, pid, status.as_deref())
        });
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "tasks": list })).capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct CrmTaskCreateTool;

impl Tool for CrmTaskCreateTool {
    fn name(&self) -> &str {
        "crm_task_create"
    }
    fn description(&self) -> &str {
        "Create a task in a project (project_id or project_name); optional status (new|progress|done, default new), dueDate (YYYY-MM-DD), isPriority (true|false, default false), recurrence (none|daily|weekly|monthly, default none), checklist ([{text, done?}], max 100 items)."
    }
    fn parameters(&self) -> Value {
        create_schema(true)
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let checklist = match parse_checklist_arg(args) {
            Ok(items) => items,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let r = with_conn(ctx, |conn| {
            let pid = resolve_project_id(conn, args)?;
            task_create_body(conn, pid, args, checklist)
        });
        if let Ok(dto) = &r {
            emit_tasks_changed(ctx, dto.project_id);
        }
        Ok(envelope(r))
    }
}

pub struct CrmTaskUpdateTool;

impl Tool for CrmTaskUpdateTool {
    fn name(&self) -> &str {
        "crm_task_update"
    }
    fn description(&self) -> &str {
        "Update fields of a task by id; absent fields stay unchanged, explicit null clears comment/dueDate/remindAt. Dates: YYYY-MM-DD, remindAt: YYYY-MM-DDTHH:MM. isPriority: true|false, recurrence: none|daily|weekly|monthly, checklist: [{text, done?}] max 100 (null/absent keeps the current value)."
    }
    fn parameters(&self) -> Value {
        update_schema()
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let patch: tasks::TaskPatch = match parse_task_patch(args) {
            Ok(p) => p,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg)),
        };
        let r = with_conn(ctx, |conn| {
            task_owner(conn, id)?;
            task_update_body(conn, id, patch)
        });
        if let Ok(dto) = &r {
            emit_tasks_changed(ctx, dto.project_id);
        }
        Ok(envelope(r))
    }
}

pub struct CrmTaskMoveTool;

impl Tool for CrmTaskMoveTool {
    fn name(&self) -> &str {
        "crm_task_move"
    }
    fn description(&self) -> &str {
        "Move a task to a kanban column (new|progress|done); optional position, appended at the column end when absent. Moving a recurring task (recurrence daily/weekly/monthly) to done queues the next occurrence in the new column."
    }
    fn parameters(&self) -> Value {
        move_schema()
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let status = args["status"].as_str().unwrap_or_default();
        let position = args.get("position").and_then(Value::as_f64);
        let r = with_conn(ctx, |conn| task_move_body(conn, id, status, position));
        if let Ok(dto) = &r {
            emit_tasks_changed(ctx, dto.project_id);
        }
        Ok(envelope(r))
    }
}

pub struct CrmTaskDeleteTool;

impl Tool for CrmTaskDeleteTool {
    fn name(&self) -> &str {
        "crm_task_delete"
    }
    fn description(&self) -> &str {
        "Delete a task by id. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        delete_schema()
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление задачи из CRM".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| task_delete_body(conn, id));
        if let Ok(pid) = &r {
            emit_tasks_changed(ctx, *pid);
            emit_trash_changed(ctx, None);
        }
        Ok(match r {
            Ok(_) => ToolResult::ok(json!({ "deleted": id })),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct DatasReadTool;

impl Tool for DatasReadTool {
    fn name(&self) -> &str {
        "datas_read"
    }
    fn description(&self) -> &str {
        "Read any project's datas.md (access notes; project_id or project_name); empty string when the file is missing."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "number" },
                "project_name": { "type": "string" }
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
        let wd = require_wd(ctx)?;
        let pid = with_conn(ctx, |conn| resolve_project_id(conn, args))?;
        let r = with_conn(ctx, |conn| read_datas(conn, &wd, pid));
        Ok(match r {
            Ok(text) => ToolResult::ok(json!({ "projectId": pid, "relPath": paths::DATAS_MD, "content": text }))
                .capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct DatasAddTool;

impl Tool for DatasAddTool {
    fn name(&self) -> &str {
        "datas_add"
    }
    fn description(&self) -> &str {
        "Add or update a \"## <title>\" section in any project's datas.md (project_id or project_name); an existing section is replaced, otherwise appended."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "number" },
                "project_name": { "type": "string" },
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
        let wd = require_wd(ctx)?;
        let pid = with_conn(ctx, |conn| resolve_project_id(conn, args))?;
        let title = match datas_title_arg(args) {
            Ok(title) => title,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg)),
        };
        let body = args["body"].as_str().unwrap_or_default();
        let r = with_conn(ctx, |conn| datas_add_body(conn, &wd, pid, &title, body));
        Ok(match r {
            Ok((updated, entry)) => ToolResult::ok(json!({ "projectId": pid, "title": title, "updated": updated, "entry": entry })),
            Err(e) => envelope_r(e),
        })
    }
}

// Scheduler tools (SPEC 5.2 #5, 5.3): the agent schedules its own future
// runs. schedule_create binds to the session that created it (ToolCtx
// session_id); a notify schedule needs no session. create/delete are
// confirmed because they make the agent run (or stop) unattended later.
// Every mutation notifies the open Harness view through the run state's
// emitter, mirroring the schedule_* commands in lib.rs.
fn emit_schedules_changed(ctx: &ToolCtx) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("schedules-changed", json!({}));
        }
    }
}

pub struct ScheduleCreateTool;

impl Tool for ScheduleCreateTool {
    fn name(&self) -> &str {
        "schedule_create"
    }
    fn description(&self) -> &str {
        "Create a schedule: kind=run_at (one-shot ISO datetime in spec), cron (5-field expression in spec), or notify (native notification, no model call). prompt is the user turn for run_at/cron and the body for notify; timezone is IANA (default: system); title is a short label for the UI (derived from the prompt when absent). Fires while the app is open."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["run_at", "cron", "notify"] },
                "spec": { "type": "string" },
                "prompt": { "type": "string" },
                "timezone": { "type": "string" },
                "title": { "type": "string" }
            },
            "required": ["kind", "spec"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some(format!(
            "создание расписания {}: агент будет запускаться сам",
            args.get("kind").and_then(Value::as_str).unwrap_or("?")
        ))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let kind = args["kind"].as_str().unwrap_or_default();
        let spec = args["spec"].as_str().unwrap_or_default();
        let prompt = args.get("prompt").and_then(Value::as_str);
        let timezone = args.get("timezone").and_then(Value::as_str);
        let title = args.get("title").and_then(Value::as_str);
        let r = with_conn(ctx, |conn| {
            let session_id = if kind == "notify" {
                ctx.session_id
            } else {
                match ctx.session_id {
                    Some(id) => Some(id),
                    None => Some(crate::harness::create_session(conn, Some("schedule"))?.id),
                }
            };
            scheduler::create(
                conn,
                &CreateIn {
                    kind,
                    spec,
                    prompt,
                    timezone,
                    session_id,
                    related_task_id: None,
                    title,
                },
                scheduler::now_utc(),
            )
        });
        if r.is_ok() {
            emit_schedules_changed(ctx);
        }
        Ok(envelope(r))
    }
}

pub struct ScheduleListTool;

impl Tool for ScheduleListTool {
    fn name(&self) -> &str {
        "schedule_list"
    }
    fn description(&self) -> &str {
        "List all schedules with title, kind, spec, prompt, status, timezone and next run."
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
        let r = with_conn(ctx, scheduler::list);
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "schedules": list })).capped(self.max_result_len()),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

pub struct SchedulePauseTool;

impl Tool for SchedulePauseTool {
    fn name(&self) -> &str {
        "schedule_pause"
    }
    fn description(&self) -> &str {
        "Pause a schedule by id (reversible; it stops firing until resumed)."
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
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| scheduler::pause(conn, id));
        if r.is_ok() {
            emit_schedules_changed(ctx);
        }
        Ok(envelope(r))
    }
}

pub struct ScheduleDeleteTool;

impl Tool for ScheduleDeleteTool {
    fn name(&self) -> &str {
        "schedule_delete"
    }
    fn description(&self) -> &str {
        "Delete a schedule by id. Dangerous: always confirmed."
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
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление расписания".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| scheduler::delete(conn, id));
        if r.is_ok() {
            emit_schedules_changed(ctx);
        }
        Ok(match r {
            Ok(_) => ToolResult::ok(json!({ "deleted": id })),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

// Harness session self-management: the agent sees its own session list and
// can rename sessions. rename_chat defaults to the session the run belongs
// to (ToolCtx session_id) and emits "sessions-changed" so the sidebar
// follows the new title without a manual reload.
pub struct ListChatsTool;

impl Tool for ListChatsTool {
    fn name(&self) -> &str {
        "list_chats"
    }
    fn description(&self) -> &str {
        "List harness sessions (id, title, lastActive), most recently active first."
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
        let r = with_conn(ctx, crate::harness::list_sessions);
        Ok(match r {
            Ok(list) => {
                let chats: Vec<Value> = list
                    .into_iter()
                    .map(|s| json!({ "id": s.id, "title": s.title, "lastActive": s.last_active_at }))
                    .collect();
                ToolResult::ok(json!({ "chats": chats })).capped(self.max_result_len())
            }
            Err(e) => envelope_r(e),
        })
    }
}

const RENAME_CHAT_MAX_TITLE_CHARS: usize = 120;

pub struct RenameChatTool;

impl Tool for RenameChatTool {
    fn name(&self) -> &str {
        "rename_chat"
    }
    fn description(&self) -> &str {
        "Rename a harness session: sessionId picks it explicitly, absent means the session this run belongs to. Title is a short single-line name."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "number" },
                "title": { "type": "string" }
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
        let title = args["title"].as_str().unwrap_or_default().trim().to_owned();
        if title.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "title is empty"));
        }
        if title.contains('\n') {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "title must be a single line"));
        }
        if title.chars().count() > RENAME_CHAT_MAX_TITLE_CHARS {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("title is over {RENAME_CHAT_MAX_TITLE_CHARS} characters"),
            ));
        }
        let session_id = match args.get("sessionId").and_then(Value::as_i64) {
            Some(id) => id,
            None => match ctx.session_id {
                Some(id) => id,
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "sessionId is required outside a session run",
                    ))
                }
            },
        };
        let r = with_conn(ctx, |conn| crate::harness::rename_session(conn, session_id, &title));
        if r.is_ok() {
            emit_sessions_changed(ctx);
        }
        Ok(envelope(r))
    }
}

// "sessions-changed" through the run state's emitter (same seam as rename_chat
// and todowrite): the sidebar follows session create/delete without a manual
// reload; a missing emitter is not an error.
pub(crate) fn emit_sessions_changed(ctx: &ToolCtx) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("sessions-changed", json!({}));
        }
    }
}

// Agent-side chat management: new_chat opens a separate session (its own
// context, the global memory stays shared) and delete_chat removes one,
// including the session the run itself lives in (the run then finishes into a
// deleted chat: transcript appends fail FK-safe and are dropped, never a
// crash). Chat management is safe, so both stay Mutating/Skip; every success
// emits "sessions-changed" so the sidebar follows immediately.
pub struct NewChatTool;

const NEW_CHAT_DEFAULT_TITLE: &str = "Новый чат";

impl Tool for NewChatTool {
    fn name(&self) -> &str {
        "new_chat"
    }
    fn description(&self) -> &str {
        "Create a new harness session: a separate chat with its own context (only the global memory is shared). Optional short single-line title, default «Новый чат». Returns the new sessionId and title."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" }
            },
            "required": [],
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
        let title = match args.get("title").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => {
                let title = raw.trim();
                if title.contains('\n') {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "title must be a single line",
                    ));
                }
                if title.chars().count() > RENAME_CHAT_MAX_TITLE_CHARS {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        format!("title is over {RENAME_CHAT_MAX_TITLE_CHARS} characters"),
                    ));
                }
                title.to_owned()
            }
            _ => NEW_CHAT_DEFAULT_TITLE.to_owned(),
        };
        let r = with_conn(ctx, |conn| crate::harness::create_session(conn, Some(&title)));
        if r.is_ok() {
            emit_sessions_changed(ctx);
        }
        Ok(match r {
            Ok(s) => ToolResult::ok(json!({ "sessionId": s.id, "title": s.title })),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct DeleteChatTool;

impl Tool for DeleteChatTool {
    fn name(&self) -> &str {
        "delete_chat"
    }
    fn description(&self) -> &str {
        "Delete a harness session and its messages: sessionId picks it explicitly, absent means the session this run belongs to (deleting the current chat is allowed; the run then finishes without persisting more). Schedules of the deleted session survive and fire into a fresh session."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessionId": { "type": "number" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        // Deleting a session and all its messages is destructive and
        // irreversible from the agent's side, so it confirms like every other
        // delete in the catalog (fs_delete, task_delete, crm_task_delete).
        PermissionPolicy::Check
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let session_id = match args.get("sessionId") {
            Some(value) => match value.as_i64() {
                Some(id) if id > 0 => id,
                _ => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "sessionId must be a positive integer",
                    ))
                }
            },
            None => match ctx.session_id {
                Some(id) => id,
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "sessionId is required outside a session run",
                    ))
                }
            },
        };
        let current = ctx.session_id == Some(session_id);
        let r = with_conn(ctx, |conn| crate::harness::delete_session(conn, session_id));
        if r.is_ok() {
            emit_sessions_changed(ctx);
        }
        Ok(match r {
            Ok(()) => ToolResult::ok(json!({ "deleted": true, "current": current })),
            Err(e) => envelope_r(e),
        })
    }
}

// Hybrid search over past harness sessions (FTS + vectors, schema v19):
// one hit per session with the best-matching message and its snippet.
pub struct SearchChatsTool;

impl Tool for SearchChatsTool {
    fn name(&self) -> &str {
        "search_chats"
    }
    fn description(&self) -> &str {
        "Search past harness sessions by message text (keyword + semantic), best hits first."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "number", "minimum": 1, "maximum": 50 }
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
        let query = args["query"].as_str().unwrap_or_default().trim().to_owned();
        if query.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "query is empty"));
        }
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let state = match state_of(ctx) {
            Ok(state) => state,
            Err(e) => return Ok(envelope_r(e)),
        };
        let r = crate::search::search_chats(state, &query, limit);
        Ok(match r {
            Ok(hits) => ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::{Resolution, LIST_TOOLS, ENABLE_TOOLS};
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::index;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::state::{AppState, UiEmitter};
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
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let recorder = RecordingEmitter::default();
        let events = recorder.events.clone();
        *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
        let ctx = ToolCtx {
            project_id: None,
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        Env { wd, ctx, events }
    }

    fn tool(name: &str) -> Arc<dyn Tool> {
        let catalog = harness_catalog();
        match catalog.resolve(name) {
            Resolution::Found(t) => t.clone(),
            _ => panic!("tool {name} missing from harness catalog"),
        }
    }

    fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        tool(name).execute(&args, ctx).unwrap()
    }

    const DEFERRED_NAMES: [&str; 56] = [
        "fs_read",
        "fs_list",
        "fs_write",
        "fs_delete",
        "fs_copy",
        "fs_move",
        "fs_rename",
        "fs_mkdir",
        "fs_grep",
        "edit_file",
        "apply_patch",
        "create_document",
        "create_spreadsheet",
        "ocr",
        "pdf_merge",
        "pdf_split",
        "pdf_fill_form",
        "image_transform",
        "screenshot",
        "create_skill",
        "bash",
        "process_start",
        "process_logs",
        "process_stop",
        "process_list",
        "browser",
        "webfetch",
        "websearch",
        "http_request",
        "crm_list_projects",
        "crm_search",
        "crm_tasks",
        "crm_task_create",
        "crm_task_update",
        "crm_task_move",
        "crm_task_delete",
        "datas_read",
        "datas_add",
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
        "schedule_create",
        "schedule_list",
        "schedule_pause",
        "schedule_delete",
    ];

    #[test]
    fn catalog_registers_everything_without_duplicate_names() {
        let mut catalog = harness_catalog();
        let active: Vec<String> = catalog.to_specs().into_iter().map(|s| s.name).collect();
        for expected in ["finish_task", "skill", "skill_exec", "remember", "recall", "set_user_gender", "forget", "list_chats", "rename_chat", "new_chat", "delete_chat", "search_chats", LIST_TOOLS, ENABLE_TOOLS] {
            assert!(active.iter().any(|n| n == expected), "missing active {expected}");
        }
        let summary = catalog.list_summary();
        let available: Vec<String> = summary["available"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect();
        for name in DEFERRED_NAMES {
            assert!(matches!(catalog.resolve(name), Resolution::Found(_)), "missing {name}");
            assert!(available.iter().any(|n| n == name), "{name} not in available");
        }
        for name in DEFERRED_NAMES {
            assert!(matches!(catalog.enable(name), super::super::catalog::EnableOutcome::Activated));
        }
    }

    #[test]
    fn classes_policies_and_danger_reasons_match_the_spec() {
        for name in ["fs_read", "fs_list", "fs_grep", "ocr", "webfetch", "websearch", "crm_list_projects", "crm_search", "crm_tasks", "datas_read", "git_status", "git_diff", "git_log", "ssh_list_profiles"] {
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
        }
        for name in ["fs_write", "fs_copy", "fs_move", "fs_rename", "fs_mkdir", "crm_task_create", "crm_task_update", "crm_task_move", "datas_add", "create_skill", "ssh_save_profile_to_datas"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
        }
        assert_eq!(tool("ssh_save_profile_to_datas").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("fs_write").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_copy").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_move").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_rename").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("fs_mkdir").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("create_skill").policy(), PermissionPolicy::Skip);
        for name in ["fs_delete", "bash", "skill_exec", "crm_task_delete", "forget", "git_commit", "git_pull", "git_push", "apply_patch", "ssh_upload", "ssh_download", "ssh_generate_key", "ssh_install_key", "screenshot"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({}), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        // ssh_exec decides per call: read-only commands skip, the rest confirm
        assert_eq!(tool("ssh_exec").class(), ToolClass::Mutating);
        assert_eq!(tool("ssh_exec").policy(), PermissionPolicy::CheckConditional);
        assert!(tool("ssh_exec").danger_reason(&json!({ "command": "ls" }), &ToolCtx::default()).is_none());
        assert!(tool("ssh_exec").danger_reason(&json!({ "command": "ls | wc -l" }), &ToolCtx::default()).is_some());
        assert!(tool("ssh_exec").danger_reason(&json!({ "command": "rm -rf /" }), &ToolCtx::default()).is_some());
        // browser decides per action: read-only actions skip, the rest confirm
        assert_eq!(tool("browser").class(), ToolClass::Mutating);
        assert_eq!(tool("browser").policy(), PermissionPolicy::CheckConditional);
        assert!(tool("browser").danger_reason(&json!({ "action": "text" }), &ToolCtx::default()).is_none());
        assert!(tool("browser").danger_reason(&json!({ "action": "open" }), &ToolCtx::default()).is_some());
        assert_eq!(tool("git_branch").class(), ToolClass::Mutating);
        assert_eq!(tool("git_branch").policy(), PermissionPolicy::CheckConditional);
        assert!(tool("git_branch").danger_reason(&json!({ "action": "list" }), &ToolCtx::default()).is_none());
        assert!(tool("git_branch").danger_reason(&json!({ "action": "create", "name": "n" }), &ToolCtx::default()).is_some());
        for name in ["schedule_create", "schedule_delete"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({ "kind": "cron" }), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        assert_eq!(tool("schedule_list").class(), ToolClass::ReadOnly);
        assert_eq!(tool("schedule_list").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("schedule_pause").class(), ToolClass::Mutating);
        assert_eq!(tool("schedule_pause").policy(), PermissionPolicy::Skip);
        for name in ["process_logs", "process_list"] {
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
        }
        for name in ["process_start", "process_stop"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({}), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        assert_eq!(tool("list_chats").class(), ToolClass::ReadOnly);
        assert_eq!(tool("list_chats").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("rename_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("rename_chat").policy(), PermissionPolicy::Skip);
        // Chat create/rename are non-destructive and skip the queue; delete
        // destroys a session and its messages, so it confirms like other deletes.
        assert_eq!(tool("new_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("new_chat").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("delete_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("delete_chat").policy(), PermissionPolicy::Check);
        assert_eq!(tool("search_chats").class(), ToolClass::ReadOnly);
        assert_eq!(tool("search_chats").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("todowrite").class(), ToolClass::Mutating);
        assert_eq!(tool("todowrite").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("todoread").class(), ToolClass::ReadOnly);
        assert_eq!(tool("todoread").policy(), PermissionPolicy::Skip);

        let env = setup("ht-policy");
        let write = tool("fs_write");
        let inside = write.danger_reason(&json!({ "path": "Harness/a.txt", "content": "x" }), &env.ctx);
        assert_eq!(inside, None, "inside Harness must not confirm");
        let inside_projects = write.danger_reason(&json!({ "path": "Projects/p/a.txt", "content": "x" }), &env.ctx);
        assert_eq!(inside_projects, None, "inside Projects must not confirm");
        let outside = write.danger_reason(&json!({ "path": "outside.txt", "content": "x" }), &env.ctx);
        assert_eq!(outside.as_deref(), Some("запись вне Harness/ и Projects/"));
        let outside_abs = write.danger_reason(&json!({ "path": "/etc/hosts", "content": "x" }), &env.ctx);
        assert!(outside_abs.is_some());

        let edit = tool("edit_file");
        assert_eq!(edit.class(), ToolClass::Mutating);
        assert_eq!(edit.policy(), PermissionPolicy::CheckConditional);
        let edit_args = |path: &str| json!({ "path": path, "old_string": "x", "new_string": "y" });
        assert!(edit.danger_reason(&edit_args("Harness/a.txt"), &env.ctx).is_none());
        assert!(edit.danger_reason(&edit_args("Projects/p/a.txt"), &env.ctx).is_none());
        assert!(edit.danger_reason(&edit_args("outside.txt"), &env.ctx).is_some());
        assert_eq!(tool("apply_patch").class(), ToolClass::Mutating);
        assert_eq!(tool("apply_patch").policy(), PermissionPolicy::Check);

        for name in ["fs_copy", "fs_move"] {
            let t = tool(name);
            let inside = json!({ "from": "Harness/a.txt", "to": "Harness/b.txt" });
            assert!(t.danger_reason(&inside, &env.ctx).is_none(), "{name}: inside->inside must not confirm");
            let into_projects = json!({ "from": "Harness/a.txt", "to": "Projects/p/b.txt" });
            assert!(t.danger_reason(&into_projects, &env.ctx).is_none(), "{name}: inside->Projects must not confirm");
            let out_to = json!({ "from": "Harness/a.txt", "to": "outside.txt" });
            assert!(t.danger_reason(&out_to, &env.ctx).is_some(), "{name}: outside target must confirm");
            let out_from = json!({ "from": "outside.txt", "to": "Harness/b.txt" });
            assert!(t.danger_reason(&out_from, &env.ctx).is_some(), "{name}: outside source must confirm");
            let forced = json!({ "from": "Harness/a.txt", "to": "Harness/b.txt", "overwrite": true });
            assert!(t.danger_reason(&forced, &env.ctx).is_some(), "{name}: overwrite must confirm even inside");
        }

        for name in ["fs_rename", "fs_mkdir"] {
            let t = tool(name);
            assert!(
                t.danger_reason(&json!({ "path": "Harness/a.txt" }), &env.ctx)
                    .is_none(),
                "{name}: inside Harness must not confirm"
            );
            assert!(
                t.danger_reason(&json!({ "path": "Projects/p/a.txt" }), &env.ctx)
                    .is_none(),
                "{name}: inside Projects must not confirm"
            );
            assert!(
                t.danger_reason(&json!({ "path": "outside.txt" }), &env.ctx)
                    .is_some(),
                "{name}: outside must confirm"
            );
            assert!(
                t.danger_reason(&json!({ "path": "/etc/cron.d" }), &env.ctx)
                    .is_some(),
                "{name}: absolute outside must confirm"
            );
        }

        let img = tool("image_transform");
        assert_eq!(img.class(), ToolClass::Mutating);
        assert_eq!(img.policy(), PermissionPolicy::CheckConditional);
        assert!(img.danger_reason(&json!({ "path": "Harness/a.png", "maxSize": 10 }), &env.ctx).is_some());
        assert!(img.danger_reason(&json!({ "path": "Harness/a.png", "output": "Harness/b.png", "maxSize": 10 }), &env.ctx).is_none());
        assert!(img.danger_reason(&json!({ "path": "Harness/a.png", "output": "outside.png", "maxSize": 10 }), &env.ctx).is_some());
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn search_chats_tool_finds_harness_messages_end_to_end() {
        let env = setup("ht-search-chats");
        let state = env.ctx.state.clone().unwrap();
        {
            let conn = state.db.lock().unwrap();
            let conn = conn.as_ref().unwrap();
            let session = crate::harness::create_session(conn, Some("витрина")).unwrap();
            let chat_id = crate::harness::session_chat_id(conn, session.id).unwrap();
            crate::chat::append_message(
                conn,
                chat_id,
                crate::llm::Role::User,
                Some("нужен редизайн главной страницы"),
                &[],
                None,
                None,
            )
            .unwrap();
        }

        let res = exec("search_chats", json!({ "query": "редизайн" }), &env.ctx);
        assert!(res.ok, "{:?}", res.error);
        let hits = res.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["sessionId"], session_id_of(&state));
        assert_eq!(hits[0]["title"], "витрина");
        assert!(hits[0]["snippet"].as_str().unwrap().contains("<<редизайн>>"));

        let empty = exec("search_chats", json!({ "query": "  " }), &env.ctx);
        assert!(!empty.ok);
        assert_eq!(empty.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    fn session_id_of(state: &Arc<AppState>) -> i64 {
        let conn = state.db.lock().unwrap();
        conn.as_ref()
            .unwrap()
            .query_row("SELECT id FROM sessions LIMIT 1", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn schedule_tools_create_list_pause_delete_round_trip() {
        let env = setup("ht-schedule");
        let r = exec(
            "schedule_create",
            json!({ "kind": "cron", "spec": "0 10 * * *", "prompt": "daily", "timezone": "Europe/Moscow" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        // no session in ctx: a dedicated harness session was created for it
        let session_id: Option<i64> = with_conn(&env.ctx, |conn| {
            conn.query_row("SELECT session_id FROM schedules WHERE id = ?1", params![id], |r| r.get(0))
                .map_err(Into::into)
        })
        .unwrap();
        assert!(session_id.is_some(), "run_at/cron schedule must bind to a session");

        let r = exec("schedule_list", json!({}), &env.ctx);
        let items = r.data.unwrap()["schedules"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "cron");
        assert_eq!(items[0]["title"], "daily", "title derived from the prompt");
        assert!(items[0]["nextRunAt"].is_string());

        let r = exec("schedule_pause", json!({ "id": id }), &env.ctx);
        assert_eq!(r.data.unwrap()["status"], "paused");

        let r = exec("schedule_delete", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("schedule_pause", json!({ "id": id }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        // notify needs no session and takes an ISO spec
        let r = exec(
            "schedule_create",
            json!({ "kind": "notify", "spec": "2030-01-01T09:00:00", "prompt": "ping", "timezone": "Europe/Moscow" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let r = exec("schedule_list", json!({}), &env.ctx);
        let items = r.data.unwrap()["schedules"].as_array().unwrap().clone();
        assert_eq!(items[0]["sessionId"], Value::Null);

        // every successful schedule mutation notifies the UI; the failed pause does not
        let events = env.events.lock().unwrap().clone();
        let notified = events
            .iter()
            .filter(|(name, _)| name == "schedules-changed")
            .count();
        assert_eq!(notified, 4, "create/pause/delete/create-notify must notify: {events:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn list_and_rename_chat_tools_round_trip() {
        let env = setup("ht-chats");
        let a = with_conn(&env.ctx, |conn| crate::harness::create_session(conn, Some("a"))).unwrap();
        let b = with_conn(&env.ctx, |conn| crate::harness::create_session(conn, None)).unwrap();

        let r = exec("list_chats", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let chats = r.data.unwrap()["chats"].as_array().unwrap().clone();
        assert_eq!(chats.len(), 2);
        // Same-second creation ties on last_active_at: the id DESC tiebreak
        // puts the newer session first, matching the UI list order.
        assert_eq!(chats[0]["id"], b.id);
        assert_eq!(chats[0]["title"], Value::Null);
        assert!(chats[0]["lastActive"].is_string());
        assert_eq!(chats[1]["id"], a.id);
        assert_eq!(chats[1]["title"], "a");

        // Explicit sessionId renames that session.
        let r = exec("rename_chat", json!({ "sessionId": a.id, "title": " Переименована " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["title"], "Переименована");

        // No sessionId: the ToolCtx session of the run is renamed.
        let ctx = ToolCtx {
            session_id: Some(b.id),
            ..env.ctx
        };
        let r = exec("rename_chat", json!({ "title": "Текущая" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["title"], "Текущая");

        // Empty / multiline / overlong titles are rejected before touching DB.
        for bad in [json!({ "title": "   " }), json!({ "title": "a\nb" }), json!({ "title": "я".repeat(121) })] {
            let r = exec("rename_chat", bad, &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }

        // Unknown session id maps to not_found; no session in ctx and no
        // sessionId is an argument error.
        let r = exec("rename_chat", json!({ "sessionId": 999, "title": "x" }), &ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = exec("rename_chat", json!({ "title": "x" }), &ToolCtx::default());
        assert_eq!(r.error.unwrap().code, "invalid_args");

        let titles: Vec<Option<String>> = with_conn(&ctx, |conn| {
            let list = crate::harness::list_sessions(conn)?;
            Ok::<Vec<Option<String>>, AppError>(list.into_iter().map(|s| s.title).collect())
        })
        .unwrap();
        assert!(titles.contains(&Some("Переименована".to_string())));
        assert!(titles.contains(&Some("Текущая".to_string())));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn new_and_delete_chat_tools_round_trip_with_events() {
        let env = setup("ht-chat-mgmt");

        // new_chat without a title: the default «Новый чат» is stored, the id
        // and title come back, and the session really exists with its chat row.
        let r = exec("new_chat", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let created = r.data.unwrap();
        let new_id = created["sessionId"].as_i64().unwrap();
        assert_eq!(created["title"], "Новый чат");
        assert!(with_conn(&env.ctx, |conn| crate::harness::session_chat_id(conn, new_id)).is_ok());

        // Explicit title is trimmed and stored; empty/blank falls back to the default.
        let r = exec("new_chat", json!({ "title": "  Витрина  " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let titled_id = r.data.unwrap()["sessionId"].as_i64().unwrap();
        let titled = with_conn(&env.ctx, crate::harness::list_sessions)
            .unwrap()
            .into_iter()
            .find(|s| s.id == titled_id)
            .unwrap();
        assert_eq!(titled.title.as_deref(), Some("Витрина"));
        let r = exec("new_chat", json!({ "title": "   " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let blank_out = r.data.unwrap();
        assert_eq!(blank_out["title"], "Новый чат");
        let blank_id = blank_out["sessionId"].as_i64().unwrap();

        // Multiline / overlong titles are rejected before touching DB.
        for bad in [json!({ "title": "a\nb" }), json!({ "title": "я".repeat(121) })] {
            let r = exec("new_chat", bad, &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }

        // delete_chat with an explicit id: deleted:true, current:false.
        let r = exec("delete_chat", json!({ "sessionId": new_id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let out = r.data.unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["current"], false);
        assert!(matches!(
            with_conn(&env.ctx, |conn| crate::harness::session_chat_id(conn, new_id)),
            Err(AppError::Invalid(_))
        ));

        // Deleting the current session (ToolCtx session_id, no sessionId arg)
        // is allowed and reports current:true.
        let ctx = ToolCtx {
            session_id: Some(titled_id),
            ..env.ctx.clone()
        };
        let r = exec("delete_chat", json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let out = r.data.unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["current"], true);

        // Unknown id maps to not_found; non-positive ids and the missing
        // default outside a session run are argument errors.
        let r = exec("delete_chat", json!({ "sessionId": 999 }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        for bad in [json!({ "sessionId": 0 }), json!({ "sessionId": -3 }), json!({ "sessionId": "x" })] {
            let r = exec("delete_chat", bad, &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args");
        }
        let r = exec("delete_chat", json!({}), &ToolCtx::default());
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // Every successful create/delete notified the UI; failures did not:
        // 3 creates + 3 deletes succeeded.
        let r = exec("delete_chat", json!({ "sessionId": blank_id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let events = env.events.lock().unwrap().clone();
        let notified = events
            .iter()
            .filter(|(name, _)| name == "sessions-changed")
            .count();
        assert_eq!(notified, 6, "3 creates + 3 deletes must notify: {events:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn resolve_path_expands_home_absolute_and_relative() {
        let wd = PathBuf::from("/wd");
        assert_eq!(resolve_path("a/b", &wd), PathBuf::from("/wd/a/b"));
        assert_eq!(resolve_path("./a/../b", &wd), PathBuf::from("/wd/b"));
        assert_eq!(resolve_path("/x/y", &wd), PathBuf::from("/x/y"));
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert_eq!(resolve_path("~/notes.md", &wd), home.join("notes.md"));
        assert_eq!(resolve_path("~", &wd), home);
    }

    #[test]
    fn fs_write_read_list_delete_round_trip_inside_and_outside() {
        let env = setup("ht-fs");
        let inside = env.wd.join("Harness").join("sub").join("note.md");
        let r = exec("fs_write", json!({ "path": "Harness/sub/note.md", "content": "v1" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(&inside).unwrap(), "v1");

        let r = exec("fs_read", json!({ "path": "Harness/sub/note.md" }), &env.ctx);
        assert_eq!(r.data.unwrap()["content"], "v1");

        // outside the territory: execute itself still works (the loop, not
        // the tool, enforces the confirmation)
        let outside = env.wd.join("outside.txt");
        let r = exec("fs_write", json!({ "path": "outside.txt", "content": "v2" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "v2");

        let r = exec("fs_list", json!({ "path": "Harness/sub" }), &env.ctx);
        let entries = r.data.unwrap()["entries"].as_array().unwrap().clone();
        assert_eq!(entries[0]["name"], "note.md");
        assert_eq!(entries[0]["kind"], "file");

        let r = exec("fs_delete", json!({ "path": "outside.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!outside.exists());
        let r = exec("fs_delete", json!({ "path": "Harness/sub" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!inside.exists());

        // obviously destructive targets are refused even with confirmation
        for bad in ["", ".", "~", "/"] {
            let r = exec("fs_delete", json!({ "path": bad }), &env.ctx);
            assert!(!r.ok, "accepted {bad:?}");
        }
        let r = exec("fs_read", json!({ "path": "missing.md" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_copy_and_move_files_dirs_collisions_and_parents() {
        let env = setup("ht-copy");
        let h = env.wd.join("Harness");

        // copy a file: the source stays, kind/bytes are reported
        fs::write(h.join("src.txt"), "hello").unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dst.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["kind"], "file");
        assert_eq!(data["bytes"], 5);
        assert_eq!(data["to"], h.join("dst.txt").display().to_string());
        assert_eq!(fs::read_to_string(h.join("dst.txt")).unwrap(), "hello");
        assert!(h.join("src.txt").is_file(), "copy must keep the source");

        // copy into an existing directory keeps the source name
        fs::create_dir_all(h.join("dir")).unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dir" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["to"], h.join("dir/src.txt").display().to_string());
        assert!(h.join("dir/src.txt").is_file());

        // collision without overwrite
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dir" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("destination exists"), "{}", err.message);

        // overwrite=true replaces the file
        fs::write(h.join("dst.txt"), "old content").unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/dst.txt", "overwrite": true }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(h.join("dst.txt")).unwrap(), "hello");

        // missing parents of the target are created
        let r = exec("fs_copy", json!({ "from": "Harness/src.txt", "to": "Harness/deep/nested/copy.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(h.join("deep/nested/copy.txt")).unwrap(), "hello");

        // recursive directory copy with byte totals
        fs::create_dir_all(h.join("tree/sub")).unwrap();
        fs::write(h.join("tree/a.txt"), "aaa").unwrap();
        fs::write(h.join("tree/sub/b.txt"), "bbbb").unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/tree", "to": "Harness/tree2" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["kind"], "dir");
        assert_eq!(data["bytes"], 7);
        assert_eq!(fs::read_to_string(h.join("tree2/sub/b.txt")).unwrap(), "bbbb");

        // a directory cannot be copied into itself or its own subfolder
        for to in ["Harness/tree", "Harness/tree/sub"] {
            let r = exec("fs_copy", json!({ "from": "Harness/tree", "to": to }), &env.ctx);
            let err = r.error.unwrap();
            assert_eq!(err.code, "invalid_args", "{to}");
            assert!(err.message.contains("itself"), "{}", err.message);
        }

        // an existing directory destination is never merged or overwritten
        let r = exec("fs_copy", json!({ "from": "Harness/tree/sub", "to": "Harness/tree2", "overwrite": true }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("directories") && err.message.contains("fs_delete"), "{}", err.message);

        // move a file: the source is gone
        let r = exec("fs_move", json!({ "from": "Harness/dst.txt", "to": "Harness/moved.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["bytes"], 5);
        assert!(!h.join("dst.txt").exists());
        assert_eq!(fs::read_to_string(h.join("moved.txt")).unwrap(), "hello");

        // move a directory
        let r = exec("fs_move", json!({ "from": "Harness/tree2", "to": "Harness/tree3" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["kind"], "dir");
        assert_eq!(fs::read_to_string(h.join("tree3/a.txt")).unwrap(), "aaa");
        assert!(!h.join("tree2").exists());

        // move collision, then overwrite
        let r = exec("fs_move", json!({ "from": "Harness/moved.txt", "to": "Harness/src.txt" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "invalid_args");
        let r = exec("fs_move", json!({ "from": "Harness/moved.txt", "to": "Harness/src.txt", "overwrite": true }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!h.join("moved.txt").exists());
        assert_eq!(fs::read_to_string(h.join("src.txt")).unwrap(), "hello");

        // non-existent source
        let r = exec("fs_copy", json!({ "from": "Harness/missing.txt", "to": "Harness/x.txt" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = exec("fs_move", json!({ "from": "Harness/missing.txt", "to": "Harness/x.txt" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_rename_files_dirs_collisions_and_invalid_names() {
        let env = setup("ht-rename");
        let h = env.wd.join("Harness");

        // rename a file inside the same parent
        fs::write(h.join("a.txt"), "hello").unwrap();
        let r = exec("fs_rename", json!({ "path": "Harness/a.txt", "newName": "b.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["from"], h.join("a.txt").display().to_string());
        assert_eq!(data["to"], h.join("b.txt").display().to_string());
        assert!(!h.join("a.txt").exists());
        assert_eq!(fs::read_to_string(h.join("b.txt")).unwrap(), "hello");

        // rename a directory: the whole tree moves with it
        fs::create_dir_all(h.join("dir/sub")).unwrap();
        fs::write(h.join("dir/sub/f.txt"), "x").unwrap();
        let r = exec("fs_rename", json!({ "path": "Harness/dir", "newName": "dir2" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(!h.join("dir").exists());
        assert_eq!(fs::read_to_string(h.join("dir2/sub/f.txt")).unwrap(), "x");

        // renaming to the same name is a no-op success
        let r = exec("fs_rename", json!({ "path": "Harness/b.txt", "newName": "b.txt" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);

        // collision with an existing entry is refused, both sides survive
        fs::write(h.join("c.txt"), "c").unwrap();
        let r = exec("fs_rename", json!({ "path": "Harness/b.txt", "newName": "c.txt" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("destination exists"), "{}", err.message);
        assert!(h.join("b.txt").is_file() && h.join("c.txt").is_file());

        // invalid names: separators, traversal, empty, control characters
        for bad in ["x/y", "..", ".", "", "a\\b", "x\u{0}y"] {
            let r = exec("fs_rename", json!({ "path": "Harness/c.txt", "newName": bad }), &env.ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "newName {bad:?}");
        }
        assert!(h.join("c.txt").is_file());

        // missing source maps to not_found
        let r = exec("fs_rename", json!({ "path": "Harness/missing.txt", "newName": "x" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        // schema: both fields required, extras refused
        let schema = tool("fs_rename").parameters();
        assert!(validate_args(&schema, &json!({ "path": "Harness/a.txt", "newName": "b" })).is_ok());
        assert!(validate_args(&schema, &json!({ "path": "Harness/a.txt" }))
            .unwrap_err()
            .contains("newName"));
        assert!(validate_args(&schema, &json!({ "path": "a", "newName": "b", "overwrite": true }))
            .unwrap_err()
            .contains("overwrite"));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn fs_mkdir_requires_existing_parent_and_refuses_existing() {
        let env = setup("ht-mkdir");
        let h = env.wd.join("Harness");

        let r = exec("fs_mkdir", json!({ "path": "Harness/newdir" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["created"], true);
        assert!(h.join("newdir").is_dir());

        // an existing folder or file is refused
        let r = exec("fs_mkdir", json!({ "path": "Harness/newdir" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("already exists"), "{}", err.message);
        fs::write(h.join("afile.txt"), "x").unwrap();
        let r = exec("fs_mkdir", json!({ "path": "Harness/afile.txt" }), &env.ctx);
        assert!(
            r.error.unwrap().message.contains("already exists as a file"),
            "must name the kind"
        );

        // parents are not created
        let r = exec("fs_mkdir", json!({ "path": "Harness/no/such/parent" }), &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("parent folder does not exist"), "{}", err.message);
        assert!(!h.join("no").exists());

        // empty path is refused
        let r = exec("fs_mkdir", json!({ "path": "   " }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // inside Projects behaves like inside Harness
        let r = exec("fs_mkdir", json!({ "path": "Projects/pdir" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);

        // schema: path required, extras refused
        let schema = tool("fs_mkdir").parameters();
        assert!(validate_args(&schema, &json!({ "path": "Harness/x" })).is_ok());
        assert!(validate_args(&schema, &json!({})).unwrap_err().contains("path"));
        assert!(validate_args(&schema, &json!({ "path": "x", "parents": true }))
            .unwrap_err()
            .contains("parents"));
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fs_copy_and_move_never_follow_symlinks_and_skip_links_in_trees() {
        let env = setup("ht-copy-symlink");
        let h = env.wd.join("Harness");
        let outside = env.wd.join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, h.join("link")).unwrap();

        // a territory link resolving outside confirms (policy sees the real
        // path) and execution refuses to follow it
        let copy = tool("fs_copy");
        let args = json!({ "from": "Harness/link/x.txt", "to": "Harness/y.txt" });
        assert!(copy.danger_reason(&args, &env.ctx).is_some(), "symlink escape must confirm");
        let r = exec("fs_copy", args, &env.ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("symlink"), "{}", err.message);
        assert!(!h.join("y.txt").exists());

        // symlinks inside a copied tree are skipped, not followed
        fs::create_dir_all(h.join("tree")).unwrap();
        fs::write(h.join("tree/real.txt"), "r").unwrap();
        std::os::unix::fs::symlink(h.join("tree/real.txt"), h.join("tree/lnk.txt")).unwrap();
        let r = exec("fs_copy", json!({ "from": "Harness/tree", "to": "Harness/tree-copy" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["bytes"], 1);
        assert!(h.join("tree-copy/real.txt").is_file());
        assert!(!h.join("tree-copy/lnk.txt").exists());
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
    fn fs_read_resolves_lenient_name_variants_and_refuses_ambiguity() {
        let env = setup("ht-lenient");
        let real = "Harness/Договор № 017-2026\u{a0}— копия.txt";
        fs::write(env.wd.join(real), "lenient body").unwrap();
        let r = exec(
            "fs_read",
            json!({ "path": "Harness/Договор № 017-2026 - копия.txt" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["content"], "lenient body");

        // a second file differing only in the dash makes the variant ambiguous
        fs::write(
            env.wd.join("Harness/Договор № 017-2026\u{a0}\u{2013} копия.txt"),
            "en",
        )
        .unwrap();
        let r = exec(
            "fs_read",
            json!({ "path": "Harness/Договор № 017-2026 - копия.txt" }),
            &env.ctx,
        );
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("ambiguous"), "{}", err.message);
    }

    #[test]
    fn fs_read_extracts_documents_caps_and_rejects_images_and_binaries() {
        let env = setup("ht-docs");
        let dir = env.wd.join("Harness").join("docs");
        fs::create_dir_all(&dir).unwrap();

        // plain text keeps the old shape: no extracted/format keys
        fs::write(dir.join("note.md"), "plain harness text").unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/note.md" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "plain harness text");
        assert!(data.get("extracted").is_none());
        assert!(data.get("format").is_none());

        // docx built in memory: extracted text plus the extracted/format keys
        fs::write(dir.join("report.docx"), docx_bytes("Harness docx body")).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/report.docx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Harness docx body");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "docx");

        // xlsx likewise
        fs::write(dir.join("sheet.xlsx"), xlsx_bytes("Q3 plan")).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/sheet.xlsx" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["content"], "Q3 plan");
        assert_eq!(data["extracted"], true);
        assert_eq!(data["format"], "xlsx");

        // images get the vision hint instead of base64
        fs::write(dir.join("pic.png"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/pic.png" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("vision-capable"), "{}", err.message);

        // unknown binary extensions keep the old clear rejection
        fs::write(dir.join("blob.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/blob.dat" }), &env.ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("binary"), "{}", err.message);

        // max_chars cap: a long docx paragraph is cut to max_result_len
        fs::write(dir.join("long.docx"), docx_bytes(&"y".repeat(9_000))).unwrap();
        let r = exec("fs_read", json!({ "path": "Harness/docs/long.docx" }), &env.ctx);
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
    fn fs_grep_finds_pattern_and_respects_max_results() {
        let env = setup("ht-grep");
        let dir = env.wd.join("Harness").join("tree");
        fs::create_dir_all(dir.join("deep")).unwrap();
        fs::write(dir.join("a.txt"), "one NEEDLE one").unwrap();
        fs::write(dir.join("deep/b.txt"), "two needle two\nthree NEEDLE three").unwrap();
        fs::write(dir.join("c.txt"), "no match").unwrap();

        let r = exec("fs_grep", json!({ "pattern": "needle" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let matches = data["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3, "{matches:?}");
        assert_eq!(data["truncated"], false);

        let r = exec("fs_grep", json!({ "pattern": "needle", "max_results": 2 }), &env.ctx);
        let data = r.data.unwrap();
        assert_eq!(data["matches"].as_array().unwrap().len(), 2);
        assert_eq!(data["truncated"], true);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn bash_runs_with_tmp_env_default_cwd_and_exit_codes() {
        let env = setup("ht-bash");
        let r = exec("bash", json!({ "command": "echo hi" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0);
        assert_eq!(data["stdout"], "hi\n");
        assert_eq!(data["cwd"], paths::harness_dir(&env.wd).display().to_string());

        let r = exec("bash", json!({ "command": "exit 3" }), &env.ctx);
        assert!(r.ok);
        assert_eq!(r.data.unwrap()["exitCode"], 3);

        let r = exec("bash", json!({ "command": "echo \"$TMPDIR|$TEMP|$TMP\"" }), &env.ctx);
        let out = r.data.unwrap()["stdout"].as_str().unwrap().to_owned();
        let tmp = paths::harness_tmp_dir(&env.wd);
        for part in out.trim().split('|') {
            assert!(!part.is_empty(), "empty tmp var in {out:?}");
            assert!(Path::new(part).ends_with("Harness/tmp"), "{part} not under Harness/tmp");
            assert_eq!(PathBuf::from(part), tmp);
        }

        let r = exec("bash", json!({ "command": "pwd", "cwd": "sub" }), &env.ctx);
        // pwd resolves symlinks (/var -> /private/var on macOS), so compare
        // against the canonicalized cwd.
        assert_eq!(
            r.data.unwrap()["stdout"].as_str().unwrap().trim(),
            env.wd.join("sub").canonicalize().unwrap().display().to_string()
        );
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn bash_timeout_kills_the_command() {
        let env = setup("ht-bash-timeout");
        let tool = BashTool::with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let r = tool.execute(&json!({ "command": "sleep 5" }), &env.ctx).unwrap();
        let elapsed = started.elapsed();
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("timed out"), "{}", err.message);
        assert!(elapsed < Duration::from_secs(4), "kill took {elapsed:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn crm_projects_search_tasks_and_datas_round_trip() {
        let env = setup("ht-crm");
        let (p1, p2) = with_conn(&env.ctx, |conn| {
            let a = projects::create_project(conn, &env.wd, "alpha")?;
            let b = projects::create_project(conn, &env.wd, "beta")?;
            Ok::<(i64, i64), AppError>((a.id, b.id))
        })
        .unwrap();

        let r = exec("crm_list_projects", json!({}), &env.ctx);
        let projects = r.data.unwrap()["projects"].as_array().unwrap().clone();
        assert_eq!(projects.len(), 2);
        assert!(projects[0]["taskCount"].is_number());

        // index one file per project, then search scoped and cross-project
        fs::write(paths::projects_dir(&env.wd).join("alpha/plan.md"), "crossword in alpha").unwrap();
        fs::write(paths::projects_dir(&env.wd).join("beta/notes.md"), "crossword in beta").unwrap();
        let state = env.ctx.state.as_deref().unwrap();
        index::index_project(state, p1).unwrap();
        index::index_project(state, p2).unwrap();

        let r = exec("crm_search", json!({ "query": "crossword", "project_name": "alpha" }), &env.ctx);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["projectId"], p1);
        assert_eq!(hits[0]["relPath"], "plan.md");

        let r = exec("crm_search", json!({ "query": "crossword" }), &env.ctx);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 2, "cross-project search: {hits:?}");
        let pids: Vec<i64> = hits.iter().map(|h| h["projectId"].as_i64().unwrap()).collect();
        assert!(pids.contains(&p1) && pids.contains(&p2));

        // task round trip
        let r = exec("crm_task_create", json!({ "project_name": "alpha", "title": "first" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        let r = exec("crm_task_update", json!({ "id": id, "title": "renamed", "comment": "note" }), &env.ctx);
        let data = r.data.unwrap();
        assert_eq!(data["title"], "renamed");
        assert_eq!(data["comment"], "note");

        let r = exec("crm_task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert_eq!(r.data.unwrap()["status"], "done");

        let r = exec("crm_tasks", json!({ "project_name": "alpha", "status": "done" }), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);

        let r = exec("crm_task_delete", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("crm_task_update", json!({ "id": id, "title": "ghost" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        // every successful task mutation notifies the UI with the owning project
        let events = env.events.lock().unwrap().clone();
        let notified: Vec<&Value> = events
            .iter()
            .filter(|(name, _)| name == "tasks-changed")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(notified.len(), 4, "create/update/move/delete must notify: {events:?}");
        assert!(
            notified.iter().all(|p| p["projectId"] == json!(p1)),
            "payload must carry the owning project id: {notified:?}"
        );

        // datas for any project
        let r = exec("datas_add", json!({ "project_name": "beta", "title": "FTP example.ru", "body": "login: user-1" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("datas_read", json!({ "project_name": "beta" }), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(content.contains("## FTP example.ru") && content.contains("user-1"));

        let r = exec("crm_tasks", json!({ "project_name": "nope" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn crm_search_opens_one_connection_for_the_whole_call() {
        // S1.2: the old shape opened one connection per project (search_project
        // per pid); now a single connection serves resolution plus all legs.
        let env = setup("ht-crm-conn");
        let mut pids = Vec::new();
        for name in ["alpha", "beta", "gamma"] {
            let dto = with_conn(&env.ctx, |conn| {
                projects::create_project(conn, &env.wd, name)
            })
            .unwrap();
            pids.push(dto.id);
        }
        for name in ["alpha", "beta", "gamma"] {
            fs::write(
                paths::projects_dir(&env.wd).join(format!("{name}/plan.md")),
                "crossword word",
            )
            .unwrap();
        }
        let state = env.ctx.state.as_deref().unwrap();
        for pid in &pids {
            index::index_project(state, *pid).unwrap();
        }

        db::open_counter::arm();
        let r = exec("crm_search", json!({ "query": "crossword" }), &env.ctx);
        let opens = db::open_counter::stop();
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 3, "cross-project hits: {hits:?}");
        let hit_pids: Vec<i64> = hits
            .iter()
            .map(|h| h["projectId"].as_i64().unwrap())
            .collect();
        assert_eq!(hit_pids.len(), hit_pids.iter().collect::<std::collections::HashSet<_>>().len());
        assert_eq!(opens, 1, "crm_search must open exactly one connection per call");

        db::open_counter::arm();
        let r = exec(
            "crm_search",
            json!({ "query": "crossword", "project_name": "beta" }),
            &env.ctx,
        );
        let opens = db::open_counter::stop();
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["projectId"], pids[1]);
        assert_eq!(hits[0]["relPath"], "plan.md");
        assert_eq!(opens, 1, "scoped crm_search must open exactly one connection");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn crm_task_tools_accept_and_validate_is_priority_due_date_checklist_recurrence() {
        let env = setup("ht-crm-upgrade");
        with_conn(&env.ctx, |conn| {
            projects::create_project(conn, &env.wd, "alpha")?;
            Ok::<(), AppError>(())
        })
        .unwrap();

        let r = exec(
            "crm_task_create",
            json!({
                "project_name": "alpha",
                "title": "report",
                "dueDate": "2099-01-02",
                "isPriority": true,
                "recurrence": "daily",
                "checklist": [{ "text": "draft", "done": true }]
            }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let id = data["id"].as_i64().unwrap();
        assert_eq!(data["isPriority"], true);
        assert_eq!(data["dueDate"], "2099-01-02");
        assert_eq!(data["recurrence"], "daily");
        assert_eq!(data["checklist"], json!([{ "text": "draft", "done": true }]));

        let r = exec("crm_task_update", json!({ "id": id, "isPriority": false }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["isPriority"], false);

        let r = exec("crm_task_create", json!({ "project_name": "alpha", "title": "x", "dueDate": "2099-13-01" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("due_date"));
        let r = exec("crm_task_update", json!({ "id": id, "recurrence": "yearly" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("recurrence"));

        // done move queues the next occurrence; crm_tasks sees both.
        let r = exec(
            "crm_task_update",
            json!({ "id": id, "isPriority": true, "dueDate": "2099-01-02" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let r = exec("crm_task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("crm_tasks", json!({ "project_name": "alpha" }), &env.ctx);
        let tasks = r.data.unwrap()["tasks"].as_array().unwrap().clone();
        assert_eq!(tasks.len(), 2);
        let next = tasks.iter().find(|t| t["status"] == "new").unwrap();
        assert_eq!(next["dueDate"], "2099-01-03");
        assert_eq!(next["isPriority"], true, "the flag must be copied to the next occurrence");
        assert_eq!(next["checklist"], json!([{ "text": "draft", "done": false }]));
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
