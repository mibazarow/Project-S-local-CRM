// Agent git tools via the system `git` CLI (no new crates). status/diff/log are
// ReadOnly/Skip in both modes; branch create/switch and commit are Mutating and
// confirmed (branch list skips confirmation through CheckConditional). Only the
// subcommands used here ever run: pull defaults to --ff-only and push is never
// forced; reset --hard, clean and force operations are not reachable from this
// module. Project mode keeps every path inside the
// project root; harness mode accepts absolute/~/working-dir-relative paths.
// All output is secret-masked (journal heuristic) and capped.
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{read_capped, ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::journal::mask_secrets;
use crate::projects;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_CAP: usize = 20_000;
// Envelope cap leaves room for the JSON wrapper around a full 20k output.
const GIT_RESULT_CAP: usize = 21_000;
const LOG_DEFAULT: f64 = 20.0;
const LOG_MAX: f64 = 100.0;
const DIFF_CONTEXT_DEFAULT: f64 = 3.0;
const DIFF_CONTEXT_MAX: f64 = 20.0;

pub(crate) struct GitOutput {
    pub(crate) code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) truncated: bool,
}

// `git -C <dir> <args>` with piped output, a hard timeout and masked output.
pub(crate) fn run_git(dir: &Path, args: &[&str]) -> Result<GitOutput> {
    let mut child = match Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(AppError::Io(format!("cannot run git: {e}"))),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, GIT_OUTPUT_CAP)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, GIT_OUTPUT_CAP)));
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!(
                "git {} timed out after {} s and was killed",
                args.first().copied().unwrap_or(""),
                GIT_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
    let (stderr, err_cut) = err_thread.join().unwrap_or_default().unwrap_or_default();
    Ok(GitOutput {
        code: status.code().unwrap_or(-1),
        stdout: mask_secrets(&stdout),
        stderr: mask_secrets(&stderr),
        truncated: out_cut || err_cut,
    })
}

// Non-zero git exit becomes an execution_error carrying stderr (then stdout);
// the label is the exact text shown before " failed" (S5.3, shared with
// edit_tools' apply_patch failures).
pub(crate) fn git_exit_fail(label: &str, out: &GitOutput) -> ToolResult {
    let detail = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    ToolResult::err(
        ToolErrorCode::ExecutionError,
        format!("{label} failed (exit {}): {detail}", out.code),
    )
}

fn git_fail(action: &str, out: &GitOutput) -> ToolResult {
    git_exit_fail(&format!("git {action}"), out)
}

// Project mode: default is the project root, any path is project-relative and
// must stay inside it. Harness mode: absolute/~/wd-relative paths, default is
// the working dir. The resolved folder must exist (git -C needs it).
fn resolve_dir(args: &Value, ctx: &ToolCtx) -> Result<PathBuf> {
    let wd = require_wd(ctx)?;
    let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, &wd, pid))?;
            if raw.is_empty() {
                return Ok(root);
            }
            let joined = crate::path_policy::join_rel(&root, raw)?;
            let canon = joined
                .canonicalize()
                .map_err(|_| AppError::Invalid(format!("\"{raw}\" not found")))?;
            if !canon.starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok(canon)
        }
        None => {
            let dir = if raw.is_empty() {
                wd
            } else {
                super::harness_tools::resolve_path(raw, &wd)
            };
            dir.canonicalize()
                .map_err(|_| AppError::Invalid(format!("\"{}\" not found", dir.display())))
        }
    }
}

fn ensure_repo(dir: &Path) -> Result<()> {
    let out = run_git(dir, &["rev-parse", "--is-inside-work-tree"])?;
    if out.code == 0 && out.stdout.trim() == "true" {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "{} is not a git repository",
            dir.display()
        )))
    }
}

// Shared static guard for every remote/branch argument: empty names, option
// lookalikes (leading "-") and force-push refspecs (leading "+") never reach
// the git option parser; whitespace and control bytes break argv quoting.
fn ref_arg_error(label: &str, name: &str) -> Option<String> {
    if name.is_empty()
        || name.starts_with('-')
        || name.starts_with('+')
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Some(format!("invalid {label} name \"{name}\""));
    }
    None
}

// Cheap static guard first (also keeps the name out of git's option parser),
// then git's own authoritative ref-format check.
fn branch_name_error(dir: &Path, name: &str) -> Option<String> {
    if let Some(msg) = ref_arg_error("branch", name) {
        return Some(msg);
    }
    match run_git(dir, &["check-ref-format", "--branch", name]) {
        Ok(out) if out.code == 0 => None,
        Ok(out) => {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim()
            } else {
                out.stderr.trim()
            };
            Some(format!("invalid branch name \"{name}\": {detail}"))
        }
        Err(e) => Some(e.to_string()),
    }
}

// Remote names are validated the same way: static guard plus git's ref-format
// check applied to the refs/remotes/<name> ref the remote maps to.
fn remote_name_error(dir: &Path, name: &str) -> Option<String> {
    if let Some(msg) = ref_arg_error("remote", name) {
        return Some(msg);
    }
    match run_git(dir, &["check-ref-format", &format!("refs/remotes/{name}")]) {
        Ok(out) if out.code == 0 => None,
        Ok(out) => {
            let detail = if out.stderr.trim().is_empty() {
                out.stdout.trim()
            } else {
                out.stderr.trim()
            };
            Some(format!("invalid remote name \"{name}\": {detail}"))
        }
        Err(e) => Some(e.to_string()),
    }
}

pub struct GitStatusTool;

impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }
    fn description(&self) -> &str {
        "Short git status (porcelain v1) of a repository. path picks the repository folder: default is the project root (project agent) or the working dir (harness agent)."
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
    fn max_result_len(&self) -> usize {
        GIT_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let out = run_git(&dir, &["status", "--porcelain=v1"])?;
        if out.code != 0 {
            return Ok(git_fail("status", &out));
        }
        let changes: Vec<&str> = out.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        Ok(ToolResult::ok(json!({
            "dir": dir.display().to_string(),
            "changes": changes,
            "clean": changes.is_empty() && !out.truncated,
            "truncated": out.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct GitDiffTool;

impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn description(&self) -> &str {
        "Unified diff of uncommitted changes; staged=true shows the index instead. context_lines sets the context (0-20, default 3). Output is capped."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "staged": { "type": "boolean" },
                "context_lines": { "type": "number" }
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
    fn max_result_len(&self) -> usize {
        GIT_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
        let context = args
            .get("context_lines")
            .and_then(Value::as_f64)
            .unwrap_or(DIFF_CONTEXT_DEFAULT)
            .clamp(0.0, DIFF_CONTEXT_MAX) as i64;
        let u = format!("-U{context}");
        let mut cmd: Vec<&str> = vec!["diff", "--no-color", u.as_str()];
        if staged {
            cmd.push("--staged");
        }
        let out = run_git(&dir, &cmd)?;
        if out.code != 0 {
            return Ok(git_fail("diff", &out));
        }
        Ok(ToolResult::ok(json!({
            "dir": dir.display().to_string(),
            "staged": staged,
            "diff": out.stdout,
            "truncated": out.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct GitLogTool;

impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }
    fn description(&self) -> &str {
        "One-line git log, newest first; limit defaults to 20, max 100. An empty repository returns an empty list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "number" }
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
    fn max_result_len(&self) -> usize {
        GIT_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(LOG_DEFAULT)
            .clamp(1.0, LOG_MAX) as i64;
        let n = format!("-n{limit}");
        let out = run_git(&dir, &["log", "--oneline", n.as_str()])?;
        if out.code != 0 {
            if out.stderr.contains("does not have any commits yet") {
                return Ok(ToolResult::ok(json!({
                    "dir": dir.display().to_string(),
                    "entries": [],
                    "count": 0,
                    "truncated": false,
                })));
            }
            return Ok(git_fail("log", &out));
        }
        let entries: Vec<&str> = out.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        Ok(ToolResult::ok(json!({
            "dir": dir.display().to_string(),
            "entries": entries,
            "count": entries.len(),
            "truncated": out.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct GitBranchTool;

impl Tool for GitBranchTool {
    fn name(&self) -> &str {
        "git_branch"
    }
    fn description(&self) -> &str {
        "List, create or switch git branches. list is read-only; create and switch are confirmed. Branch names are validated with git check-ref-format."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "action": { "type": "string", "enum": ["list", "create", "switch"] },
                "name": { "type": "string" }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        match args.get("action").and_then(Value::as_str) {
            Some("create") => Some("создание ветки в git-репозитории".to_string()),
            Some("switch") => Some("переключение ветки в git-репозитории".to_string()),
            _ => None,
        }
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let action = args["action"].as_str().unwrap_or("");
        if action == "list" {
            let out = run_git(&dir, &["branch", "--list"])?;
            if out.code != 0 {
                return Ok(git_fail("branch", &out));
            }
            let branches: Vec<&str> = out.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
            return Ok(ToolResult::ok(json!({
                "dir": dir.display().to_string(),
                "branches": branches,
                "truncated": out.truncated,
            }))
            .capped(self.max_result_len()));
        }
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        if let Some(msg) = branch_name_error(&dir, name) {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
        }
        let (sub, label) = match action {
            "create" => ("branch", "created"),
            "switch" => ("switch", "switched"),
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("unknown action \"{action}\""),
                ))
            }
        };
        let out = run_git(&dir, &[sub, name])?;
        if out.code != 0 {
            return Ok(git_fail(sub, &out));
        }
        let mut data = json!({ label: name });
        if action == "switch" {
            let cur = run_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            if cur.code != 0 {
                return Ok(git_fail("rev-parse", &cur));
            }
            data["current"] = json!(cur.stdout.trim());
        }
        Ok(ToolResult::ok(data))
    }
}

pub struct GitCommitTool;

impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }
    fn description(&self) -> &str {
        "Commit changes: git add -A (addAll=true, the default) or git add -u (tracked files only), then git commit -m. Returns the short hash and the git show --stat --oneline summary. Never pushes."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "message": { "type": "string" },
                "addAll": { "type": "boolean" }
            },
            "required": ["message"],
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
        Some("коммит в git-репозиторий".to_string())
    }
    fn max_result_len(&self) -> usize {
        GIT_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let message = args["message"].as_str().unwrap_or("").trim();
        if message.is_empty() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "commit message is empty",
            ));
        }
        let add_all = args.get("addAll").and_then(Value::as_bool).unwrap_or(true);
        let add = run_git(&dir, &["add", if add_all { "-A" } else { "-u" }])?;
        if add.code != 0 {
            return Ok(git_fail("add", &add));
        }
        let commit = run_git(&dir, &["commit", "-m", message])?;
        if commit.code != 0 {
            return Ok(git_fail("commit", &commit));
        }
        let show = run_git(&dir, &["show", "--stat", "--oneline", "-1"])?;
        if show.code != 0 {
            return Ok(git_fail("show", &show));
        }
        let hash = show.stdout.split_whitespace().next().unwrap_or_default();
        Ok(ToolResult::ok(json!({
            "hash": hash,
            "summary": show.stdout.trim(),
            "truncated": show.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct GitPullTool;

impl Tool for GitPullTool {
    fn name(&self) -> &str {
        "git_pull"
    }
    fn description(&self) -> &str {
        "git pull in a repository, fast-forward only by default (--ff-only); allowMerge=true permits a merge pull. remote and branch are optional validated names."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "remote": { "type": "string" },
                "branch": { "type": "string" },
                "allowMerge": { "type": "boolean" }
            },
            "required": [],
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
        Some("git pull: обновление рабочей копии из удалённого репозитория".to_string())
    }
    fn max_result_len(&self) -> usize {
        GIT_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let remote = args.get("remote").and_then(Value::as_str).unwrap_or("");
        let branch = args.get("branch").and_then(Value::as_str).unwrap_or("");
        if !remote.is_empty() {
            if let Some(msg) = remote_name_error(&dir, remote) {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
            }
        }
        if !branch.is_empty() {
            if let Some(msg) = branch_name_error(&dir, branch) {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
            }
        }
        let allow_merge = args.get("allowMerge").and_then(Value::as_bool).unwrap_or(false);
        let mut cmd: Vec<&str> = vec!["pull"];
        if !allow_merge {
            cmd.push("--ff-only");
        }
        if !remote.is_empty() {
            cmd.push(remote);
        }
        if !branch.is_empty() {
            cmd.push(branch);
        }
        let before = run_git(&dir, &["rev-parse", "HEAD"])?;
        if before.code != 0 {
            return Ok(git_fail("rev-parse", &before));
        }
        let out = run_git(&dir, &cmd)?;
        if out.code != 0 {
            return Ok(git_fail("pull", &out));
        }
        let after = run_git(&dir, &["rev-parse", "HEAD"])?;
        if after.code != 0 {
            return Ok(git_fail("rev-parse", &after));
        }
        let first_line = out.stdout.lines().next().unwrap_or("").trim();
        let summary = if first_line.starts_with("Already up to date") {
            first_line.to_string()
        } else {
            out.stdout.trim().to_string()
        };
        let mut data = json!({
            "summary": summary,
            "stdout": out.stdout,
            "truncated": out.truncated,
        });
        if before.stdout.trim() == after.stdout.trim() {
            data["alreadyUpToDate"] = json!(true);
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

pub struct GitPushTool;

impl Tool for GitPushTool {
    fn name(&self) -> &str {
        "git_push"
    }
    fn description(&self) -> &str {
        "git push of the current or named branch to a remote (default remote when omitted). Never force-pushes; setUpstream=true adds -u and needs branch."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "remote": { "type": "string" },
                "branch": { "type": "string" },
                "setUpstream": { "type": "boolean" }
            },
            "required": [],
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
        Some("git push: публикация коммитов в удалённый репозиторий".to_string())
    }
    fn max_result_len(&self) -> usize {
        GIT_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let dir = match resolve_dir(args, ctx) {
            Ok(d) => d,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Err(e) = ensure_repo(&dir) {
            return Ok(envelope::<()>(Err(e)));
        }
        let remote = args.get("remote").and_then(Value::as_str).unwrap_or("");
        let branch = args.get("branch").and_then(Value::as_str).unwrap_or("");
        if !remote.is_empty() {
            if let Some(msg) = remote_name_error(&dir, remote) {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
            }
        }
        if !branch.is_empty() {
            if let Some(msg) = branch_name_error(&dir, branch) {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
            }
        }
        let set_upstream = args.get("setUpstream").and_then(Value::as_bool).unwrap_or(false);
        let mut cmd: Vec<&str> = vec!["push"];
        if set_upstream && !branch.is_empty() {
            cmd.push("-u");
        }
        if !remote.is_empty() {
            cmd.push(remote);
        }
        if !branch.is_empty() {
            cmd.push(branch);
        }
        let out = run_git(&dir, &cmd)?;
        if out.code != 0 {
            return Ok(git_fail("push", &out));
        }
        let detail = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        let mut data = json!({
            "summary": detail,
            "stdout": out.stdout,
            "truncated": out.truncated,
        });
        if detail.contains("Everything up-to-date") {
            data["upToDate"] = json!(true);
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

// Checked once per test run; every git test skips when git is missing.
#[cfg(test)]
pub(crate) fn git_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| Command::new("git").arg("--version").output().is_ok())
}

#[cfg(test)]
pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git available");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[cfg(test)]
mod tests {
    use super::super::tool::validate_args;
    use super::*;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::state::AppState;
    use std::fs;
    use std::sync::Arc;

    // Local config keeps commits working regardless of the user's global git
    // settings (identity, gpg signing); pull.rebase=false keeps merge pulls
    // deterministic on top of user config.
    fn init_repo(tag: &str) -> PathBuf {
        let dir = temp_test_dir(tag);
        git_ok(&dir, &["init", "-q"]);
        git_ok(&dir, &["config", "user.email", "test@example.com"]);
        git_ok(&dir, &["config", "user.name", "Test User"]);
        git_ok(&dir, &["config", "commit.gpgsign", "false"]);
        git_ok(&dir, &["config", "pull.rebase", "false"]);
        dir
    }

    // Bare local remote for pull/push tests (file:// URL keeps paths with
    // spaces safe and never looks like an option).
    fn bare_remote(tag: &str) -> PathBuf {
        let dir = temp_test_dir(tag);
        git_ok(&dir, &["init", "-q", "--bare"]);
        dir
    }

    fn clone_with_config(url: &str, dest: &Path) {
        let parent = dest.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        git_ok(parent, &["clone", "-q", url, &dest.to_string_lossy()]);
        git_ok(dest, &["config", "user.email", "test@example.com"]);
        git_ok(dest, &["config", "user.name", "Test User"]);
        git_ok(dest, &["config", "commit.gpgsign", "false"]);
        git_ok(dest, &["config", "pull.rebase", "false"]);
    }

    // Harness mode: working dir is the repo, paths may be absolute.
    fn harness_ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    // Project mode: a real CRM project under Projects/.
    fn project_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            project_id: Some(dto.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        (paths::projects_dir(&wd).join("proj"), ctx)
    }

    fn exec(tool: &dyn Tool, args: Value, ctx: &ToolCtx) -> ToolResult {
        tool.execute(&args, ctx).unwrap()
    }

    #[test]
    fn git_exit_fail_prefers_stderr_then_stdout_with_exact_text() {
        let out = GitOutput {
            code: 128,
            stdout: "out text".into(),
            stderr: "bad thing".into(),
            truncated: false,
        };
        let err = git_exit_fail("git apply", &out).error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert_eq!(err.message, "git apply failed (exit 128): bad thing");
        let out = GitOutput {
            code: 1,
            stdout: "out text".into(),
            stderr: "  \n".into(),
            truncated: false,
        };
        let err = git_exit_fail("git apply --check", &out).error.unwrap();
        assert_eq!(err.message, "git apply --check failed (exit 1): out text");
    }

    #[test]
    fn git_fail_prefixes_the_action_with_git() {
        let out = GitOutput {
            code: 128,
            stdout: String::new(),
            stderr: "not a git repository".into(),
            truncated: false,
        };
        let err = git_fail("rev-parse", &out).error.unwrap();
        assert_eq!(err.message, "git rev-parse failed (exit 128): not a git repository");
    }

    #[test]
    fn status_diff_and_log_round_trip() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("gt-basic");
        let ctx = harness_ctx(&repo);
        fs::write(repo.join("a.txt"), "one\n").unwrap();

        let r = exec(&GitStatusTool, json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["changes"].as_array().unwrap(), &vec![json!("?? a.txt")]);
        assert_eq!(data["clean"], false);

        git_ok(&repo, &["add", "a.txt"]);
        git_ok(&repo, &["commit", "-m", "first"]);

        fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let r = exec(&GitDiffTool, json!({ "context_lines": 0 }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let diff = r.data.unwrap()["diff"].as_str().unwrap().to_owned();
        assert!(diff.contains("+two"), "{diff}");
        assert!(
            !diff.lines().any(|l| l == " one"),
            "context_lines=0 must drop context lines: {diff}"
        );

        git_ok(&repo, &["add", "a.txt"]);
        let r = exec(&GitDiffTool, json!({ "staged": true }), &ctx);
        assert!(r.data.unwrap()["diff"].as_str().unwrap().contains("+two"));
        let r = exec(&GitDiffTool, json!({}), &ctx);
        assert_eq!(r.data.unwrap()["diff"].as_str().unwrap(), "");

        git_ok(&repo, &["commit", "-m", "second"]);
        let r = exec(&GitLogTool, json!({}), &ctx);
        let entries = r.data.unwrap()["entries"].as_array().unwrap().clone();
        let lines: Vec<String> = entries.iter().map(|e| e.as_str().unwrap().to_owned()).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" second") && lines[1].ends_with(" first"), "{lines:?}");
        let r = exec(&GitLogTool, json!({ "limit": 1 }), &ctx);
        assert_eq!(r.data.unwrap()["count"], 1);

        // oversized output is capped with a truncated flag
        fs::write(repo.join("big.txt"), "x".repeat(30_000)).unwrap();
        git_ok(&repo, &["add", "big.txt"]);
        let r = exec(&GitDiffTool, json!({ "staged": true }), &ctx);
        assert_eq!(r.data.unwrap()["truncated"], true);
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn empty_repo_log_returns_empty_list() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("gt-empty-log");
        let ctx = harness_ctx(&repo);
        let r = exec(&GitLogTool, json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(r.data.unwrap()["entries"].as_array().unwrap().is_empty());
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn commit_returns_hash_and_honours_add_all() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("gt-commit");
        let ctx = harness_ctx(&repo);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        fs::write(repo.join("b.txt"), "b\n").unwrap();

        let r = exec(&GitCommitTool, json!({ "message": "init both" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let hash = data["hash"].as_str().unwrap();
        assert!(hash.len() >= 7 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "{hash}");
        assert_eq!(git_ok(&repo, &["rev-parse", "--short", "HEAD"]).trim(), hash);
        let summary = data["summary"].as_str().unwrap();
        assert!(summary.contains("a.txt") && summary.contains("b.txt"), "{summary}");

        // addAll=false: only tracked modifications go in, untracked stays out.
        fs::write(repo.join("a.txt"), "a2\n").unwrap();
        fs::write(repo.join("c.txt"), "c\n").unwrap();
        let r = exec(
            &GitCommitTool,
            json!({ "message": "tracked only", "addAll": false }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let status = git_ok(&repo, &["status", "--porcelain=v1"]);
        assert!(status.contains("?? c.txt"), "{status}");
        assert!(!status.contains("a.txt"), "{status}");

        // empty message never reaches git
        let r = exec(&GitCommitTool, json!({ "message": "   " }), &ctx);
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // secrets in returned output are masked
        fs::write(repo.join("d.txt"), "d\n").unwrap();
        let r = exec(
            &GitCommitTool,
            json!({ "message": "add token: hunter2" }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let summary = r.data.unwrap()["summary"].as_str().unwrap().to_owned();
        assert!(!summary.contains("hunter2"), "secret leaked: {summary}");
        assert!(summary.contains("***"), "{summary}");
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn non_repo_and_missing_path_are_clear_errors() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let dir = temp_test_dir("gt-nonrepo");
        let ctx = harness_ctx(&dir);
        let r = exec(&GitStatusTool, json!({}), &ctx);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert!(err.message.contains("not a git repository"), "{}", err.message);

        let r = exec(&GitStatusTool, json!({ "path": "missing-dir" }), &ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_mode_defaults_to_root_and_rejects_escapes() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let (root, ctx) = project_env("gt-project");
        git_ok(&root, &["init", "-q"]);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/f.txt"), "f\n").unwrap();

        let r = exec(&GitStatusTool, json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let changes = r.data.unwrap()["changes"].as_array().unwrap().clone();
        assert!(changes.contains(&json!("?? sub/")), "{changes:?}");

        let r = exec(&GitStatusTool, json!({ "path": "sub" }), &ctx);
        assert!(r.ok, "{:?}", r.error);

        for bad in ["../x", "/etc", "a/../../x", ".."] {
            let r = exec(&GitStatusTool, json!({ "path": bad }), &ctx);
            assert!(!r.ok, "accepted {bad:?}");
            assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
        }
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn branch_create_list_and_switch_round_trip() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("gt-branch");
        let ctx = harness_ctx(&repo);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        git_ok(&repo, &["add", "a.txt"]);
        git_ok(&repo, &["commit", "-m", "first"]);
        let initial = git_ok(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_owned();

        let r = exec(&GitBranchTool, json!({ "action": "list" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let branches = r.data.unwrap()["branches"].as_array().unwrap().clone();
        assert_eq!(branches.len(), 1);
        assert!(branches[0].as_str().unwrap().contains(&initial));

        let r = exec(
            &GitBranchTool,
            json!({ "action": "create", "name": "feature-x" }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["created"], "feature-x");

        let r = exec(&GitBranchTool, json!({ "action": "switch", "name": "feature-x" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["current"], "feature-x");
        assert_eq!(
            git_ok(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "feature-x"
        );

        // invalid names are rejected before touching the branch list
        for bad in ["bad name", "-x", "a..b", ""] {
            let r = exec(&GitBranchTool, json!({ "action": "create", "name": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
        }
        // switching to a missing branch is a git-level execution error
        let r = exec(&GitBranchTool, json!({ "action": "switch", "name": "nope" }), &ctx);
        assert_eq!(r.error.unwrap().code, "execution_error");

        // list never confirms, create/switch always do
        assert!(GitBranchTool.danger_reason(&json!({ "action": "list" }), &ctx).is_none());
        assert!(GitBranchTool
            .danger_reason(&json!({ "action": "create", "name": "n" }), &ctx)
            .is_some());
        assert!(GitBranchTool
            .danger_reason(&json!({ "action": "switch", "name": "n" }), &ctx)
            .is_some());
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn push_and_pull_round_trip_with_bare_remote() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let remote = bare_remote("gt-sync-remote");
        let url = format!("file://{}", remote.display());
        let clone = temp_test_dir("gt-sync-clone").join("work");
        clone_with_config(&url, &clone);
        let ctx = harness_ctx(&clone);

        fs::write(clone.join("a.txt"), "a\n").unwrap();
        git_ok(&clone, &["add", "a.txt"]);
        git_ok(&clone, &["commit", "-m", "first"]);
        let branch = git_ok(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_owned();
        let r = exec(&GitPushTool, json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let summary = r.data.unwrap()["summary"].as_str().unwrap().to_owned();
        assert!(
            summary.contains("[new branch]") && summary.contains(&format!("{branch} -> {branch}")),
            "{summary}"
        );

        // second clone sees only the first commit, then pulls the second
        let other = temp_test_dir("gt-sync-other").join("work");
        clone_with_config(&url, &other);
        let other_ctx = harness_ctx(&other);

        fs::write(clone.join("b.txt"), "b\n").unwrap();
        git_ok(&clone, &["add", "b.txt"]);
        git_ok(&clone, &["commit", "-m", "second"]);
        let r = exec(&GitPushTool, json!({ "remote": "origin" }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let summary = r.data.unwrap()["summary"].as_str().unwrap().to_owned();
        assert!(summary.contains(&format!("{branch} -> {branch}")), "{summary}");

        let r = exec(&GitPullTool, json!({}), &other_ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert!(data.get("alreadyUpToDate").is_none(), "{data}");
        assert_eq!(
            git_ok(&other, &["rev-parse", "HEAD"]).trim(),
            git_ok(&clone, &["rev-parse", "HEAD"]).trim()
        );

        // nothing new to pull: alreadyUpToDate flag
        let branch = git_ok(&other, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_owned();
        let r = exec(&GitPullTool, json!({ "remote": "origin", "branch": branch }), &other_ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["alreadyUpToDate"], true);
        assert!(data["summary"].as_str().unwrap().contains("Already up to date"));

        // push with nothing to publish
        let r = exec(&GitPushTool, json!({}), &ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["upToDate"], true);

        // setUpstream needs a branch and sets the tracking ref
        git_ok(&clone, &["branch", "feature"]);
        let r = exec(
            &GitPushTool,
            json!({ "remote": "origin", "branch": "feature", "setUpstream": true }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            git_ok(&clone, &["rev-parse", "--abbrev-ref", "feature@{upstream}"]).trim(),
            "origin/feature"
        );
        fs::remove_dir_all(remote).unwrap();
        fs::remove_dir_all(clone.parent().unwrap()).unwrap();
        fs::remove_dir_all(other.parent().unwrap()).unwrap();
    }

    #[test]
    fn pull_ff_only_refuses_divergence_and_allow_merge_merges() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let remote = bare_remote("gt-diverge-remote");
        let url = format!("file://{}", remote.display());
        let c1 = temp_test_dir("gt-diverge-c1").join("work");
        clone_with_config(&url, &c1);
        fs::write(c1.join("a.txt"), "a\n").unwrap();
        git_ok(&c1, &["add", "a.txt"]);
        git_ok(&c1, &["commit", "-m", "base"]);
        git_ok(&c1, &["push", "-q", "origin", "HEAD"]);

        let c2 = temp_test_dir("gt-diverge-c2").join("work");
        clone_with_config(&url, &c2);
        let ctx2 = harness_ctx(&c2);

        fs::write(c1.join("a.txt"), "remote\n").unwrap();
        git_ok(&c1, &["commit", "-aqm", "remote"]);
        git_ok(&c1, &["push", "-q", "origin", "HEAD"]);
        // different file: the merge pull must succeed cleanly
        fs::write(c2.join("b.txt"), "local\n").unwrap();
        git_ok(&c2, &["add", "b.txt"]);
        git_ok(&c2, &["commit", "-m", "local"]);

        // default pull is --ff-only and refuses with a clear git error
        let r = exec(&GitPullTool, json!({}), &ctx2);
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(
            err.message.contains("Not possible to fast-forward")
                || err.message.contains("not allow fast-forward")
                || err.message.contains("Must be fast-forward"),
            "{}",
            err.message
        );
        assert!(
            !err.message.contains("--force"),
            "force flag leaked: {}",
            err.message
        );

        // allowMerge=true performs a real merge pull
        let r = exec(&GitPullTool, json!({ "allowMerge": true }), &ctx2);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            git_ok(&c2, &["rev-list", "--count", "HEAD"]).trim(),
            "4",
            "merge pull must combine both sides (base + remote + local + merge)"
        );
        assert_eq!(
            git_ok(&c2, &["rev-parse", "HEAD^2"]).trim(),
            git_ok(&c1, &["rev-parse", "HEAD"]).trim()
        );
        fs::remove_dir_all(remote).unwrap();
        fs::remove_dir_all(c1.parent().unwrap()).unwrap();
        fs::remove_dir_all(c2.parent().unwrap()).unwrap();
    }

    #[test]
    fn sync_tools_never_build_force_flags_and_validate_names() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("gt-sync-guard");
        let ctx = harness_ctx(&repo);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        git_ok(&repo, &["add", "a.txt"]);
        git_ok(&repo, &["commit", "-m", "first"]);

        // force/mirror/delete lookalikes are not in the schema and are
        // rejected as unknown properties before execute ever runs
        for tool in [&GitPullTool as &dyn Tool, &GitPushTool] {
            for extra in ["force", "forceWithLease", "mirror", "delete", "flags"] {
                let err = validate_args(&tool.parameters(), &json!({ extra: true })).unwrap_err();
                assert!(err.contains(extra), "{extra}: {err}");
            }
        }

        // option lookalikes and force-push refspecs are rejected as invalid
        // names before any network command is built
        for bad in ["--force", "--force-with-lease", "--mirror", "--delete"] {
            let r = exec(&GitPushTool, json!({ "remote": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
            let r = exec(&GitPullTool, json!({ "remote": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
        }
        for bad in ["+main", "+main:main", "+HEAD"] {
            let r = exec(&GitPushTool, json!({ "branch": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
        }
        for bad in ["bad name", "-x", "a..b"] {
            let r = exec(&GitPullTool, json!({ "branch": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
            let r = exec(&GitPushTool, json!({ "branch": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
        }
        for bad in ["bad name", "-x", ".."] {
            let r = exec(&GitPullTool, json!({ "remote": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
            let r = exec(&GitPushTool, json!({ "remote": bad }), &ctx);
            assert_eq!(r.error.unwrap().code, "invalid_args", "{bad:?}");
        }
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn sync_tools_report_non_repo_and_project_escapes() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let dir = temp_test_dir("gt-sync-nonrepo");
        let ctx = harness_ctx(&dir);
        for tool in [&GitPullTool as &dyn Tool, &GitPushTool] {
            let r = exec(tool, json!({}), &ctx);
            assert!(!r.ok);
            assert!(
                r.error.unwrap().message.contains("not a git repository"),
                "not a git repository"
            );
        }
        fs::remove_dir_all(&dir).unwrap();

        let (root, ctx) = project_env("gt-sync-project");
        git_ok(&root, &["init", "-q"]);
        for bad in ["../x", "/etc", ".."] {
            for tool in [&GitPullTool as &dyn Tool, &GitPushTool] {
                let r = exec(tool, json!({ "path": bad }), &ctx);
                assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
            }
        }
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn classes_and_policies_match_the_spec() {
        let read_only: [&dyn Tool; 3] = [&GitStatusTool, &GitDiffTool, &GitLogTool];
        for t in read_only {
            assert_eq!(t.class(), ToolClass::ReadOnly);
            assert_eq!(t.policy(), PermissionPolicy::Skip);
        }
        assert_eq!(GitBranchTool.class(), ToolClass::Mutating);
        assert_eq!(GitBranchTool.policy(), PermissionPolicy::CheckConditional);
        assert_eq!(GitCommitTool.class(), ToolClass::Mutating);
        assert_eq!(GitCommitTool.policy(), PermissionPolicy::Check);
        assert!(GitCommitTool
            .danger_reason(&json!({ "message": "m" }), &ToolCtx::default())
            .is_some());
        for t in [&GitPullTool as &dyn Tool, &GitPushTool] {
            assert_eq!(t.class(), ToolClass::Mutating, "{} class", t.name());
            assert_eq!(t.policy(), PermissionPolicy::Check, "{} policy", t.name());
            assert!(
                t.danger_reason(&json!({}), &ToolCtx::default())
                    .is_some(),
                "{} danger_reason",
                t.name()
            );
        }
    }
}
