// Harness tool set (SPEC 5.2 items 1, 2, 4, 5): machine-wide fs tools, a bash
// tool, CRM tools, and the scheduler tools. Unlike the project tools, harness
// paths are absolute, ~-expanded, or relative to the working dir; the default
// territory is Harness/. fs_write skips confirmation only inside Harness/ and
// Projects/ (CheckConditional); fs_delete and bash are always confirmed — the
// confirmation is the guard, there is no path allowlist. Child processes of
// bash always see TMPDIR/TEMP/TMP pointed at Harness/tmp/ (SPEC 5.2), never
// the system /tmp. The CDP browser tool lives in browser_tool.rs.
use super::catalog::Catalog;
use super::project_tools::{
    envelope, read_datas, require_wd, state_of, upsert_section, with_conn, write_datas,
};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::scheduler::{self, CreateIn};
use crate::{paths, projects, search, tasks};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GREP_SCANNED_FILES: usize = 10_000;
const GREP_SKIP_DIRS: [&str; 3] = ["node_modules", "target", ".git"];
const LIST_MAX_ENTRIES: usize = 500;
const BASH_TIMEOUT_SECS: u64 = 120;
const BASH_OUTPUT_CAP: usize = 30_000;

// Core (finish/skill/memory) stays active from the first round; the
// machine-wide set is registered for lazy discovery (SPEC 6.4) so the
// harness widens its own schema list with enable_tools.
pub fn harness_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let core: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::agent::finish::FinishTaskTool),
        Arc::new(crate::skills::SkillTool),
        Arc::new(crate::memory::RememberTool),
        Arc::new(crate::memory::RecallTool),
        Arc::new(crate::memory::SetUserGenderTool),
        Arc::new(ListChatsTool),
        Arc::new(RenameChatTool),
    ];
    let deferred: Vec<Arc<dyn Tool>> = vec![
        Arc::new(FsReadTool),
        Arc::new(FsListTool),
        Arc::new(FsWriteTool),
        Arc::new(FsDeleteTool),
        Arc::new(FsGrepTool),
        Arc::new(crate::skills::CreateSkillTool),
        Arc::new(BashTool::default()),
        Arc::new(super::browser_tool::BrowserTool),
        Arc::new(super::net_tools::WebfetchTool::new()),
        Arc::new(super::net_tools::WebsearchTool::new()),
        Arc::new(CrmListProjectsTool),
        Arc::new(CrmSearchTool),
        Arc::new(CrmTasksTool),
        Arc::new(CrmTaskCreateTool),
        Arc::new(CrmTaskUpdateTool),
        Arc::new(CrmTaskMoveTool),
        Arc::new(CrmTaskDeleteTool),
        Arc::new(DatasReadTool),
        Arc::new(DatasAddTool),
        Arc::new(ScheduleCreateTool),
        Arc::new(ScheduleListTool),
        Arc::new(SchedulePauseTool),
        Arc::new(ScheduleDeleteTool),
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
fn canonicalish(path: &Path) -> PathBuf {
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

fn task_owner(conn: &Connection, id: i64) -> Result<i64> {
    conn.query_row("SELECT project_id FROM tasks WHERE id = ?1", params![id], |r| r.get(0))
        .map_err(|_| AppError::Invalid(format!("task {id} not found")))
}

pub struct FsReadTool;

impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a text file anywhere on the machine (absolute, ~/ or working-dir-relative path). Binary and >2 MiB files are rejected."
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
            Some(p) if !p.is_empty() => resolve_path(p, &wd),
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
        if let Ok(meta) = fs::metadata(&path) {
            if meta.is_dir() {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, format!("{} is a directory", path.display())));
            }
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return Ok(io_fail(parent, &e));
            }
        }
        match fs::write(&path, content) {
            Ok(()) => Ok(ToolResult::ok(json!({
                "path": path.display().to_string(),
                "bytesWritten": content.len(),
            }))),
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
        let r = if meta.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match r {
            Ok(()) => Ok(ToolResult::ok(json!({ "deleted": path.display().to_string() }))),
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
            Some(p) if !p.is_empty() => resolve_path(p, &wd),
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
        let pids: Vec<i64> = if scoped {
            vec![with_conn(ctx, |conn| resolve_project_id(conn, args))?]
        } else {
            with_conn(ctx, |conn| {
                let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id")?;
                let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
                Ok::<Vec<i64>, AppError>(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })?
        };
        let mut hits: Vec<Value> = Vec::new();
        for pid in pids {
            for hit in search::search_project(state, pid, query, limit)? {
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
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "number" },
                "project_name": { "type": "string" },
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
        let status = args.get("status").and_then(Value::as_str).map(str::to_owned);
        let r = with_conn(ctx, |conn| {
            let pid = resolve_project_id(conn, args)?;
            let all = tasks::tasks_list(conn, pid)?;
            Ok::<Vec<tasks::TaskDto>, AppError>(
                all.into_iter()
                    .filter(|t| status.as_deref().map_or(true, |s| t.status == s))
                    .collect(),
            )
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
        "Create a task in a project (project_id or project_name); optional status (new|progress|done, default new)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "number" },
                "project_name": { "type": "string" },
                "title": { "type": "string" },
                "status": { "type": "string", "enum": ["new", "progress", "done"] }
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
        let title = args["title"].as_str().unwrap_or_default().to_owned();
        let status = args.get("status").and_then(Value::as_str).map(str::to_owned);
        let r = with_conn(ctx, |conn| {
            let pid = resolve_project_id(conn, args)?;
            tasks::task_create(conn, pid, &title, status)
        });
        Ok(envelope(r))
    }
}

pub struct CrmTaskUpdateTool;

impl Tool for CrmTaskUpdateTool {
    fn name(&self) -> &str {
        "crm_task_update"
    }
    fn description(&self) -> &str {
        "Update fields of a task by id; absent fields stay unchanged, explicit null clears comment/dueDate/remindAt. Dates: YYYY-MM-DD, remindAt: YYYY-MM-DDTHH:MM."
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
                "tags": { "type": "array", "items": { "type": "string" } }
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
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let patch: tasks::TaskPatch = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string())),
        };
        let r = with_conn(ctx, |conn| {
            task_owner(conn, id)?;
            tasks::task_update(conn, id, patch)
        });
        Ok(envelope(r))
    }
}

pub struct CrmTaskMoveTool;

impl Tool for CrmTaskMoveTool {
    fn name(&self) -> &str {
        "crm_task_move"
    }
    fn description(&self) -> &str {
        "Move a task to a kanban column (new|progress|done); optional position, appended at the column end when absent."
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
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let status = args["status"].as_str().unwrap_or_default().to_owned();
        let position = args.get("position").and_then(Value::as_f64);
        let r = with_conn(ctx, |conn| {
            let pid = task_owner(conn, id)?;
            let position = match position {
                Some(p) => p,
                None => conn.query_row(
                    "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks WHERE project_id = ?1 AND status = ?2",
                    params![pid, status],
                    |row| row.get(0),
                )?,
            };
            tasks::task_move(conn, id, &status, position)
        });
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
        Some("удаление задачи из CRM".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| {
            task_owner(conn, id)?;
            tasks::task_delete(conn, id)
        });
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
        let title = args["title"].as_str().unwrap_or_default().trim().to_owned();
        if title.is_empty() || title.starts_with('#') || title.contains('\n') {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "title must be a single plain line"));
        }
        let body = args["body"].as_str().unwrap_or_default().to_owned();
        let r = with_conn(ctx, |conn| {
            let current = read_datas(conn, &wd, pid)?;
            let (updated, new_md) = upsert_section(&current, &title, &body);
            write_datas(conn, &wd, pid, &new_md).map(|entry| (updated, entry))
        });
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
pub struct ScheduleCreateTool;

impl Tool for ScheduleCreateTool {
    fn name(&self) -> &str {
        "schedule_create"
    }
    fn description(&self) -> &str {
        "Create a schedule: kind=run_at (one-shot ISO datetime in spec), cron (5-field expression in spec), or notify (native notification, no model call). prompt is the user turn for run_at/cron and the body for notify; timezone is IANA (default: system). Fires while the app is open."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["run_at", "cron", "notify"] },
                "spec": { "type": "string" },
                "prompt": { "type": "string" },
                "timezone": { "type": "string" }
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
                },
                scheduler::now_utc(),
            )
        });
        Ok(envelope(r))
    }
}

pub struct ScheduleListTool;

impl Tool for ScheduleListTool {
    fn name(&self) -> &str {
        "schedule_list"
    }
    fn description(&self) -> &str {
        "List all schedules with kind, spec, prompt, status, timezone and next run."
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
            if let Ok(state) = state_of(ctx) {
                if let Some(app) = state.emitter.lock().unwrap().as_ref() {
                    let _ = app.emit("sessions-changed", json!({}));
                }
            }
        }
        Ok(envelope(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::{Resolution, LIST_TOOLS, ENABLE_TOOLS};
    use crate::db;
    use crate::index;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::state::AppState;
    use std::path::PathBuf;

    struct Env {
        wd: PathBuf,
        ctx: ToolCtx,
    }

    fn setup(tag: &str) -> Env {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            project_id: None,
            session_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
        };
        Env { wd, ctx }
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

    const DEFERRED_NAMES: [&str; 23] = [
        "fs_read",
        "fs_list",
        "fs_write",
        "fs_delete",
        "fs_grep",
        "create_skill",
        "bash",
        "browser",
        "webfetch",
        "websearch",
        "crm_list_projects",
        "crm_search",
        "crm_tasks",
        "crm_task_create",
        "crm_task_update",
        "crm_task_move",
        "crm_task_delete",
        "datas_read",
        "datas_add",
        "schedule_create",
        "schedule_list",
        "schedule_pause",
        "schedule_delete",
    ];

    #[test]
    fn catalog_registers_everything_without_duplicate_names() {
        let mut catalog = harness_catalog();
        let active: Vec<String> = catalog.to_specs().into_iter().map(|s| s.name).collect();
        for expected in ["finish_task", "skill", "remember", "recall", "set_user_gender", "list_chats", "rename_chat", LIST_TOOLS, ENABLE_TOOLS] {
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
        for name in ["fs_read", "fs_list", "fs_grep", "webfetch", "websearch", "crm_list_projects", "crm_search", "crm_tasks", "datas_read"] {
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
        }
        for name in ["fs_write", "crm_task_create", "crm_task_update", "crm_task_move", "datas_add", "create_skill"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
        }
        assert_eq!(tool("fs_write").policy(), PermissionPolicy::CheckConditional);
        assert_eq!(tool("create_skill").policy(), PermissionPolicy::Skip);
        for name in ["fs_delete", "bash", "crm_task_delete"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({}), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        for name in ["schedule_create", "schedule_delete"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(tool(name).danger_reason(&json!({ "kind": "cron" }), &ToolCtx::default()).is_some(), "{name} danger_reason");
        }
        assert_eq!(tool("schedule_list").class(), ToolClass::ReadOnly);
        assert_eq!(tool("schedule_list").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("schedule_pause").class(), ToolClass::Mutating);
        assert_eq!(tool("schedule_pause").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("list_chats").class(), ToolClass::ReadOnly);
        assert_eq!(tool("list_chats").policy(), PermissionPolicy::Skip);
        assert_eq!(tool("rename_chat").class(), ToolClass::Mutating);
        assert_eq!(tool("rename_chat").policy(), PermissionPolicy::Skip);

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
        fs::remove_dir_all(&env.wd).unwrap();
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
}
