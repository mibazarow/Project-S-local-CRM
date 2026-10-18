// Machine-wide fs tools (SPEC 5.2 items 1, 2): fs_read/fs_list/fs_grep are
// read-only and skip confirmation; fs_write/fs_delete/fs_copy/fs_move/
// fs_rename/fs_mkdir mutate. Paths are absolute, ~-expanded or working-dir
// relative; the confirmation policy keys off in_default_territory (mod.rs).
// The shared fs_copy/fs_move body lives in transfer.rs.
use crate::agent::project_tools::{
    emit_trash_changed, emit_versions_changed, require_wd, with_conn,
};
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::transfer::fs_transfer;
use super::{
    canonicalish, envelope_r, in_default_territory, lenient_existing, lenient_fail, normalize,
    resolve_path, IMAGE_READ_HINT,
};
use crate::error::Result;
use crate::{extract, fs_ops, paths, undo};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GREP_SCANNED_FILES: usize = 10_000;
const GREP_SKIP_DIRS: [&str; 3] = ["node_modules", "target", ".git"];
const LIST_MAX_ENTRIES: usize = 500;

// Envelope headroom for extracted documents: the JSON wrapper around the
// content (keys, path, escaping) costs a little, so the extracted text is
// capped below max_result_len to keep the capped envelope inside the limit.
const EXTRACT_ENVELOPE_RESERVE: usize = 512;

// Minimal sanity guard for fs_delete; the confirmation remains the real one.
fn is_obviously_destructive(path: &Path, wd: &Path) -> bool {
    let resolved = canonicalish(path);
    let base = wd.canonicalize().unwrap_or_else(|_| normalize(wd));
    resolved == Path::new("/")
        || resolved == normalize(&crate::path_policy::home_dir())
        || resolved == base
        || resolved == base.join(paths::HARNESS_DIR)
        || resolved == base.join(paths::PROJECTS_DIR)
}

pub(crate) fn io_fail(path: &Path, e: &std::io::Error) -> ToolResult {
    if e.kind() == std::io::ErrorKind::NotFound {
        ToolResult::err(ToolErrorCode::NotFound, format!("{} not found", path.display()))
    } else {
        ToolResult::err(ToolErrorCode::ExecutionError, format!("{}: {e}", path.display()))
    }
}

// Resolves the parent folders of a create/write target leniently, keeping
// the final component as requested; a genuinely missing parent chain is
// returned unchanged so the caller creates it.
fn lenient_target_parent(path: &Path) -> Result<PathBuf> {
    crate::path_policy::lenient_parent(path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::{exec, setup, tool};
    use crate::agent::tool::validate_args;
    use serde_json::json;
    use std::fs;
    use std::io::Write as _;

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
}
