// Precise file-editing tools: edit_file does an exact string replacement,
// apply_patch applies a unified diff through the system `git` CLI (check
// first, then apply; --unsafe-paths is never used). Project mode keeps every
// path inside the project root; harness mode accepts absolute/~/working-dir-
// relative paths, and edit_file skips confirmation only inside Harness/ and
// Projects/ (CheckConditional), while apply_patch is always confirmed (Check).
// Patch files live under Harness/tmp/, the patch is capped at 1 MiB, and all
// git output is secret-masked and capped by git_tools::run_git (30 s timeout).
use super::git_tools::{run_git, GitOutput};
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::paths;
use crate::projects;
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_PATCH_BYTES: usize = 1024 * 1024;

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// Project-relative join: only Normal components are allowed.
fn join_rel(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for part in Path::new(rel).components() {
        match part {
            Component::Normal(name) => out.push(name),
            _ => {
                return Err(AppError::Invalid(format!(
                    "path \"{rel}\" is not project-relative"
                )))
            }
        }
    }
    Ok(out)
}

fn io_fail(path: &Path, e: &std::io::Error) -> ToolResult {
    if e.kind() == std::io::ErrorKind::NotFound {
        ToolResult::err(ToolErrorCode::NotFound, format!("{} not found", path.display()))
    } else {
        ToolResult::err(ToolErrorCode::ExecutionError, format!("{}: {e}", path.display()))
    }
}

// (label, existing target): the project-relative path as given in project
// mode, the resolved absolute path in harness mode.
fn resolve_target(args: &Value, ctx: &ToolCtx, wd: &Path) -> Result<(String, PathBuf)> {
    let raw = args["path"].as_str().unwrap_or_default();
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            let joined = join_rel(&root, raw)?;
            let canon = joined
                .canonicalize()
                .map_err(|_| AppError::Invalid(format!("\"{raw}\" not found")))?;
            if !canon.starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok((raw.to_owned(), canon))
        }
        None => {
            let path = super::harness_tools::resolve_path(raw, wd);
            Ok((path.display().to_string(), path))
        }
    }
}

pub struct EditFileTool {
    harness: bool,
}

impl EditFileTool {
    pub fn project() -> Self {
        EditFileTool { harness: false }
    }
    pub fn harness() -> Self {
        EditFileTool { harness: true }
    }
}

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Edit a text file by exact string replacement: old_string must occur exactly once unless replace_all is true; zero matches is an error. The file is rewritten atomically (temp file + rename in the same dir). path is project-relative (project agent) or absolute/~/working-dir-relative (harness agent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        if self.harness {
            PermissionPolicy::CheckConditional
        } else {
            PermissionPolicy::Skip
        }
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        if !self.harness {
            return None;
        }
        let wd = ctx.working_dir.as_deref()?;
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = super::harness_tools::resolve_path(raw, wd);
        (!super::harness_tools::in_default_territory(&path, wd))
            .then(|| "правка файла вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let old = args["old_string"].as_str().unwrap_or_default();
        let new = args["new_string"].as_str().unwrap_or_default();
        let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        if old.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "old_string is empty"));
        }
        let (label, path) = match resolve_target(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if path.is_dir() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("{} is a directory", path.display()),
            ));
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        if bytes.contains(&0) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("{label} looks binary; edit_file only handles text files"),
            ));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("{label} is not valid UTF-8"),
            ));
        };
        let found = text.matches(old).count();
        if found == 0 {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                format!("old_string not found in {label}; read the file and copy the exact text (whitespace and line breaks matter), or use a shorter unique fragment"),
            ));
        }
        if found > 1 && !replace_all {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("ambiguous: {found} matches of old_string in {label}; add more surrounding context or set replace_all=true"),
            ));
        }
        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        let replacements = if replace_all { found } else { 1 };
        let written = match write_atomic(&path, updated.as_bytes()) {
            Ok(n) => n,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        Ok(ToolResult::ok(json!({
            "path": label,
            "replacements": replacements,
            "bytes": written,
        })))
    }
}

// Temp file in the same directory, then rename: readers never see a half
// written file, and a failed rename leaves the original untouched.
fn write_atomic(path: &Path, content: &[u8]) -> Result<usize> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let tmp = parent.join(format!(".{name}.edit-tmp-{}", nanos()));
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(content.len())
}

pub struct ApplyPatchTool;

// File paths referenced by a unified diff: --- / +++ headers plus git
// rename/copy lines. Hunk bodies are skipped so removed content lines can
// never be mistaken for headers.
fn patch_paths(patch: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if line.starts_with("diff --git") {
            in_hunk = false;
        }
        if in_hunk {
            continue;
        }
        let rest = if let Some(r) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            r
        } else if let Some(r) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("rename to "))
        {
            r
        } else {
            continue;
        };
        let mut p = rest.split('\t').next().unwrap_or("");
        if p.starts_with('"') && p.ends_with('"') && p.len() >= 2 {
            p = &p[1..p.len() - 1];
        }
        if p.is_empty() || p == "/dev/null" {
            continue;
        }
        p = p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p);
        out.push(p.to_owned());
    }
    out
}

// First patch path that leaves the allowed root, if any: absolute paths,
// ".." and "." components are rejected before git ever sees the patch.
fn escaping_patch_path(patch: &str, root: &Path) -> Option<String> {
    for p in patch_paths(patch) {
        if Path::new(&p).components().all(|c| matches!(c, Component::Normal(_))) {
            continue;
        }
        return Some(format!(
            "patch path \"{p}\" is not allowed: every patched file must stay inside {}",
            root.display()
        ));
    }
    None
}

// (cwd for git apply, root every patched path must stay inside).
fn resolve_base(args: &Value, ctx: &ToolCtx, wd: &Path) -> Result<(PathBuf, PathBuf)> {
    let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            if raw.is_empty() {
                return Ok((root.clone(), root));
            }
            let joined = join_rel(&root, raw)?;
            let canon = joined
                .canonicalize()
                .map_err(|_| AppError::Invalid(format!("\"{raw}\" not found")))?;
            if !canon.starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok((canon, root))
        }
        None => {
            let dir = if raw.is_empty() {
                wd.to_path_buf()
            } else {
                super::harness_tools::resolve_path(raw, wd)
            };
            let canon = dir
                .canonicalize()
                .map_err(|_| AppError::Invalid(format!("\"{}\" not found", dir.display())))?;
            Ok((canon.clone(), canon))
        }
    }
}

fn git_apply_fail(label: &str, out: &GitOutput) -> ToolResult {
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

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply a unified diff via git apply (--whitespace=nowarn): --check runs first, so nothing changes unless the whole patch applies. Patch paths must stay inside the base folder; absolute and .. paths are rejected and --unsafe-paths is never used. path picks the base folder: default is the project root (project agent) or the working dir (harness agent). Patch is capped at 1 MiB."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "patch": { "type": "string" }
            },
            "required": ["patch"],
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
        Some("применение unified diff через git apply (может менять много файлов)".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let patch = args["patch"].as_str().unwrap_or_default();
        if patch.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "patch is empty"));
        }
        if patch.len() > MAX_PATCH_BYTES {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("patch is {} bytes, over the {MAX_PATCH_BYTES}-byte cap", patch.len()),
            ));
        }
        let (base, root) = match resolve_base(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        if let Some(msg) = escaping_patch_path(patch, &root) {
            return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, msg));
        }
        let tmp_dir = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp_dir)?;
        let patch_file = tmp_dir.join(format!("apply-patch-{}.patch", nanos()));
        fs::write(&patch_file, patch)?;
        let patch_arg = patch_file.to_string_lossy().into_owned();
        let check = run_git(&base, &["apply", "--whitespace=nowarn", "--check", &patch_arg])?;
        if check.code != 0 {
            let _ = fs::remove_file(&patch_file);
            return Ok(git_apply_fail("git apply --check", &check));
        }
        let applied = run_git(&base, &["apply", "--whitespace=nowarn", &patch_arg])?;
        let _ = fs::remove_file(&patch_file);
        if applied.code != 0 {
            return Ok(git_apply_fail("git apply", &applied));
        }
        Ok(ToolResult::ok(json!({
            "dir": base.display().to_string(),
            "applied": true,
            "files": patch_paths(patch),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn git_available() -> bool {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| Command::new("git").arg("--version").output().is_ok())
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
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

    fn init_repo(tag: &str) -> PathBuf {
        let dir = temp_test_dir(tag);
        git_ok(&dir, &["init", "-q"]);
        git_ok(&dir, &["config", "user.email", "test@example.com"]);
        git_ok(&dir, &["config", "user.name", "Test User"]);
        git_ok(&dir, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn harness_ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

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
    fn edit_file_exact_replace_ambiguous_not_found_binary_and_escapes() {
        let (root, ctx) = project_env("et-edit");
        fs::write(root.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let tool = EditFileTool::project();

        let r = exec(
            &tool,
            json!({ "path": "f.txt", "old_string": "two", "new_string": "TWO" }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["path"], "f.txt");
        assert_eq!(data["replacements"], 1);
        assert_eq!(data["bytes"], 14);
        assert_eq!(
            fs::read_to_string(root.join("f.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        // the temp file was renamed away, nothing is left behind
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("edit-tmp"))
            .collect();
        assert!(leftovers.is_empty());

        // ambiguous without replace_all, all occurrences with it
        fs::write(root.join("dup.txt"), "x x x").unwrap();
        let r = exec(
            &tool,
            json!({ "path": "dup.txt", "old_string": "x", "new_string": "y" }),
            &ctx,
        );
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("ambiguous: 3"), "{}", err.message);
        let r = exec(
            &tool,
            json!({ "path": "dup.txt", "old_string": "x", "new_string": "y", "replace_all": true }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["replacements"], 3);
        assert_eq!(fs::read_to_string(root.join("dup.txt")).unwrap(), "y y y");

        // missing old_string: not_found with a hint
        let r = exec(
            &tool,
            json!({ "path": "f.txt", "old_string": "nope", "new_string": "z" }),
            &ctx,
        );
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("not found"), "{}", err.message);
        assert!(err.message.contains("exact text"), "{}", err.message);

        // missing file: not_found
        let r = exec(
            &tool,
            json!({ "path": "missing.txt", "old_string": "a", "new_string": "b" }),
            &ctx,
        );
        assert_eq!(r.error.unwrap().code, "not_found");

        // binary files are refused
        fs::write(root.join("bin.dat"), [1u8, 2, 0, 3]).unwrap();
        let r = exec(
            &tool,
            json!({ "path": "bin.dat", "old_string": "a", "new_string": "b" }),
            &ctx,
        );
        assert_eq!(r.error.unwrap().code, "execution_error");

        // empty old_string never matches everything
        let r = exec(
            &tool,
            json!({ "path": "f.txt", "old_string": "", "new_string": "z" }),
            &ctx,
        );
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // project containment: escapes are permission_denied
        for bad in ["../x", "/etc/passwd", "a/../../x", ".."] {
            let r = exec(
                &tool,
                json!({ "path": bad, "old_string": "a", "new_string": "b" }),
                &ctx,
            );
            assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
        }
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn edit_file_harness_mode_round_trip_and_territory() {
        let wd = temp_test_dir("et-harness");
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        let ctx = harness_ctx(&wd);
        let tool = EditFileTool::harness();

        fs::write(wd.join("Harness/note.md"), "alpha beta").unwrap();
        let r = exec(
            &tool,
            json!({ "path": "Harness/note.md", "old_string": "beta", "new_string": "gamma" }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap()["path"],
            wd.join("Harness/note.md").display().to_string()
        );
        assert_eq!(fs::read_to_string(wd.join("Harness/note.md")).unwrap(), "alpha gamma");

        // CheckConditional: inside Harness/ no confirmation, outside confirms;
        // execute itself still works either way (the loop enforces the queue).
        assert!(tool
            .danger_reason(&json!({ "path": "Harness/note.md" }), &ctx)
            .is_none());
        fs::write(wd.join("out.txt"), "one").unwrap();
        assert!(tool
            .danger_reason(&json!({ "path": "out.txt" }), &ctx)
            .is_some());
        let r = exec(
            &tool,
            json!({ "path": "out.txt", "old_string": "one", "new_string": "two" }),
            &ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(wd.join("out.txt")).unwrap(), "two");

        let r = exec(
            &tool,
            json!({ "path": "missing.txt", "old_string": "a", "new_string": "b" }),
            &ctx,
        );
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn apply_patch_modifies_and_deletes_tracked_files() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("et-apply");
        let ctx = harness_ctx(&repo);
        fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(repo.join("b.txt"), "delete me\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-m", "base"]);

        // produce a real unified diff: modify a.txt, delete b.txt
        fs::write(repo.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        fs::remove_file(repo.join("b.txt")).unwrap();
        let patch = git_ok(&repo, &["diff"]);
        git_ok(&repo, &["checkout", "--", "."]);
        assert!(repo.join("b.txt").is_file());

        let r = exec(&ApplyPatchTool, json!({ "patch": patch }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["applied"], true);
        assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "one\nTWO\nthree\n");
        assert!(!repo.join("b.txt").exists());

        // the temp patch file under Harness/tmp was cleaned up
        let tmp = paths::harness_tmp_dir(&repo);
        let leftovers: Vec<_> = fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".patch"))
            .collect();
        assert!(leftovers.is_empty());
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn apply_patch_malformed_returns_git_stderr() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("et-malformed");
        let ctx = harness_ctx(&repo);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-m", "base"]);

        let r = exec(
            &ApplyPatchTool,
            json!({ "patch": "this is not a patch at all\n" }),
            &ctx,
        );
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("git apply --check failed"), "{}", err.message);

        let r = exec(&ApplyPatchTool, json!({ "patch": "" }), &ctx);
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn apply_patch_rejects_escaping_and_absolute_paths() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let repo = init_repo("et-escape");
        let ctx = harness_ctx(&repo);
        for patch in [
            "--- a/../evil.txt\n+++ b/evil.txt\n@@ -0,0 +1 @@\n+bad\n",
            "--- a/ok.txt\n+++ /tmp/evil.txt\n@@ -0,0 +1 @@\n+bad\n",
            "--- a/ok.txt\n+++ b/./nested/../x.txt\n@@ -0,0 +1 @@\n+bad\n",
        ] {
            let r = exec(&ApplyPatchTool, json!({ "patch": patch }), &ctx);
            let err = r.error.unwrap();
            assert_eq!(err.code, "permission_denied", "{patch}");
            assert!(err.message.contains("must stay inside"), "{}", err.message);
        }
        assert!(!repo.join("evil.txt").exists());
        assert!(!repo.join("x.txt").exists());
        fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn apply_patch_project_mode_defaults_to_root_and_rejects_escapes() {
        if !git_available() {
            eprintln!("git not available, skipping");
            return;
        }
        let (root, ctx) = project_env("et-apply-project");
        git_ok(&root, &["init", "-q"]);
        git_ok(&root, &["config", "user.email", "test@example.com"]);
        git_ok(&root, &["config", "user.name", "Test User"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "base"]);

        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
        let r = exec(&ApplyPatchTool, json!({ "patch": patch }), &ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap()["dir"],
            root.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "two\n");

        let r = exec(
            &ApplyPatchTool,
            json!({ "path": "../", "patch": patch }),
            &ctx,
        );
        assert_eq!(r.error.unwrap().code, "permission_denied");

        let r = exec(
            &ApplyPatchTool,
            json!({ "patch": "--- a/../out.txt\n+++ b/out.txt\n@@ -0,0 +1 @@\n+x\n" }),
            &ctx,
        );
        assert_eq!(r.error.unwrap().code, "permission_denied");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn apply_patch_size_cap() {
        let dir = temp_test_dir("et-cap");
        let ctx = harness_ctx(&dir);
        let big = "x".repeat(MAX_PATCH_BYTES + 1);
        let r = exec(&ApplyPatchTool, json!({ "patch": big }), &ctx);
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("cap"), "{}", err.message);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn classes_policies_and_danger_reasons_match_the_spec() {
        let project_edit = EditFileTool::project();
        assert_eq!(project_edit.class(), ToolClass::Mutating);
        assert_eq!(project_edit.policy(), PermissionPolicy::Skip);
        assert!(project_edit
            .danger_reason(&json!({ "path": "a.txt" }), &ToolCtx::default())
            .is_none());

        let harness_edit = EditFileTool::harness();
        assert_eq!(harness_edit.class(), ToolClass::Mutating);
        assert_eq!(harness_edit.policy(), PermissionPolicy::CheckConditional);
        // no working dir in ctx: nothing to check, no confirmation reason
        assert!(harness_edit
            .danger_reason(&json!({ "path": "a.txt" }), &ToolCtx::default())
            .is_none());

        assert_eq!(ApplyPatchTool.class(), ToolClass::Mutating);
        assert_eq!(ApplyPatchTool.policy(), PermissionPolicy::Check);
        assert!(ApplyPatchTool
            .danger_reason(&json!({}), &ToolCtx::default())
            .is_some());
    }

    #[test]
    fn schemas_reject_missing_and_unknown_props() {
        let s = EditFileTool::project().parameters();
        assert!(validate_args(
            &s,
            &json!({ "path": "a", "old_string": "x", "new_string": "y" })
        )
        .is_ok());
        assert!(validate_args(
            &s,
            &json!({ "path": "a", "old_string": "x", "new_string": "y", "replace_all": true })
        )
        .is_ok());
        assert!(validate_args(&s, &json!({ "path": "a", "old_string": "x" }))
            .unwrap_err()
            .contains("new_string"));
        assert!(validate_args(
            &s,
            &json!({ "path": "a", "old_string": "x", "new_string": "y", "bogus": 1 })
        )
        .unwrap_err()
        .contains("bogus"));

        let s = ApplyPatchTool.parameters();
        assert!(validate_args(&s, &json!({ "patch": "d" })).is_ok());
        assert!(validate_args(&s, &json!({ "patch": "d", "path": "sub" })).is_ok());
        assert!(validate_args(&s, &json!({}))
            .unwrap_err()
            .contains("patch"));
    }
}
