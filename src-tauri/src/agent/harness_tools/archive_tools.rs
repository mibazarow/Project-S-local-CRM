// Archive create/extract (Q.md computer-control slice): zip/tar.gz/7z via
// the system zip/unzip/tar/7z CLIs. Every call is an argv array (no shell);
// sources are resolved to absolute paths, validated to exist, and archived
// under their own names (the command runs with cwd pinned to each source's
// parent, tar uses -C pairs) so extraction never rebuilds absolute path
// trees. Extract lists the members first and refuses zip-slip (absolute or
// ".." entries); an existing archive or a non-empty extract dir is only
// replaced with overwrite=true. create without overwrite skips
// confirmation; extract and every overwrite confirm. A missing binary is a
// not_found error with an install hint.
use super::system_tools::run_cmd;
use crate::agent::project_tools::require_wd;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::path_policy::resolve_path;
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(120);
const FORMATS: [&str; 3] = ["zip", "tar.gz", "7z"];
const MAX_LIST_ENTRIES: usize = 20_000;

pub(crate) fn find_binary(name: &str, extra: &[&str]) -> Option<PathBuf> {
    extra
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(name))
                    .find(|p| p.is_file())
            })
        })
}

fn binary_for(format: &str, extract: bool) -> std::result::Result<PathBuf, ToolResult> {
    let (name, extra, hint) = match (format, extract) {
        ("zip", false) => ("zip", vec!["/usr/bin/zip"], "the zip command ships with every macOS install"),
        ("zip", true) => ("unzip", vec!["/usr/bin/unzip"], "the unzip command ships with every macOS install"),
        ("tar.gz", _) => ("tar", vec!["/usr/bin/tar"], "the tar command ships with every macOS install"),
        ("7z", _) => ("7z", vec!["/opt/homebrew/bin/7z", "/usr/local/bin/7z"], "install it with `brew install p7zip`"),
        _ => unreachable!("the format is validated before the lookup"),
    };
    find_binary(name, &extra).ok_or_else(|| {
        ToolResult::err(ToolErrorCode::NotFound, format!("{name} not found; {hint}"))
    })
}

// ---------------------------------------------------------------------------
// Pure plan builders: validated args + resolved paths -> exact argv.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CreatePlan {
    format: &'static str,
    dest: PathBuf,
    sources: Vec<PathBuf>,
}

// The declared FORMATS are 'static, so a matching name can be re-widened.
fn static_format(name: &str) -> Option<&'static str> {
    FORMATS.into_iter().find(|f| *f == name)
}

fn parse_create(args: &Value, wd: &Path) -> std::result::Result<CreatePlan, String> {
    let format = args
        .get("format")
        .and_then(Value::as_str)
        .ok_or("\"format\" is required for create (zip|tar.gz|7z)")?;
    let format = static_format(format)
        .ok_or_else(|| format!("format must be one of {}", FORMATS.join(", ")))?;
    let raw_sources = args
        .get("sources")
        .and_then(Value::as_array)
        .ok_or("\"sources\" must be a non-empty array of paths")?;
    if raw_sources.is_empty() {
        return Err("\"sources\" must contain at least one path".into());
    }
    let mut sources: Vec<PathBuf> = Vec::new();
    for raw in raw_sources {
        let text = raw.as_str().ok_or("\"sources\" entries must be strings")?;
        if text.trim().is_empty() {
            return Err("\"sources\" entries must not be empty".into());
        }
        let path = resolve_path(text, wd);
        if fs::symlink_metadata(&path).is_err() {
            return Err(format!("source not found: {}", path.display()));
        }
        if path.file_name().is_none() {
            return Err(format!("cannot archive the root path {}", path.display()));
        }
        sources.push(path);
    }
    let dest_raw = args
        .get("dest")
        .and_then(Value::as_str)
        .ok_or("\"dest\" is required")?;
    if dest_raw.trim().is_empty() {
        return Err("\"dest\" must not be empty".into());
    }
    let dest = resolve_path(dest_raw, wd);
    let name = dest
        .file_name()
        .ok_or("\"dest\" must name a file")?
        .to_string_lossy()
        .into_owned();
    let want_ext = match format {
        "zip" => ".zip",
        "7z" => ".7z",
        _ => ".tar.gz",
    };
    if !name.ends_with(want_ext) {
        return Err(format!(
            "\"dest\" must end with {want_ext} for format {format} (got \"{name}\")"
        ));
    }
    Ok(CreatePlan {
        format,
        dest,
        sources,
    })
}

// (cwd, argv) per CLI invocation: zip/7z append, so each source runs once
// from its own parent folder and lands in the archive under its own name;
// tar takes every source with a -C pair in one run.
fn create_invocations(plan: &CreatePlan) -> Vec<(Option<PathBuf>, Vec<String>)> {
    let dest = plan.dest.display().to_string();
    match plan.format {
        "tar.gz" => {
            let mut argv = vec!["-czf".to_string(), dest];
            for src in &plan.sources {
                argv.push("-C".into());
                argv.push(src.parent().unwrap_or(Path::new("/")).display().to_string());
                argv.push(src.file_name().unwrap_or_default().to_string_lossy().into_owned());
            }
            vec![(None, argv)]
        }
        format => plan
            .sources
            .iter()
            .map(|src| {
                let cwd = src.parent().unwrap_or(Path::new("/")).to_path_buf();
                let name = src.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let argv = if format == "zip" {
                    vec!["-q".to_string(), "-r".to_string(), dest.clone(), name]
                } else {
                    vec!["a".to_string(), "-bd".to_string(), dest.clone(), name]
                };
                (Some(cwd), argv)
            })
            .collect(),
    }
}

#[derive(Debug)]
struct ExtractPlan {
    format: &'static str,
    archive: PathBuf,
    dest: PathBuf,
}

fn detect_format(name: &str) -> Option<&'static str> {
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some("tar.gz")
    } else if name.ends_with(".zip") {
        Some("zip")
    } else if name.ends_with(".7z") {
        Some("7z")
    } else {
        None
    }
}

fn parse_extract(args: &Value, wd: &Path) -> std::result::Result<ExtractPlan, String> {
    let raw_sources = args
        .get("sources")
        .and_then(Value::as_array)
        .ok_or("\"sources\" must be an array with the one archive to extract")?;
    if raw_sources.len() != 1 {
        return Err("\"sources\" must contain exactly one archive path for extract".into());
    }
    let raw = raw_sources[0].as_str().ok_or("\"sources\" entries must be strings")?;
    let archive = resolve_path(raw, wd);
    match fs::symlink_metadata(&archive) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(format!("{} is not a file", archive.display())),
        Err(_) => return Err(format!("archive not found: {}", archive.display())),
    }
    let format = match args.get("format") {
        None | Some(Value::Null) => detect_format(
            archive
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_str(),
        )
        .ok_or_else(|| {
            format!(
                "cannot detect the archive format of {}; pass format explicitly",
                archive.display()
            )
        })?,
        Some(v) => v
            .as_str()
            .and_then(static_format)
            .ok_or_else(|| format!("format must be one of {}", FORMATS.join(", ")))?,
    };
    let dest_raw = args
        .get("dest")
        .and_then(Value::as_str)
        .ok_or("\"dest\" is required (the extract folder)")?;
    if dest_raw.trim().is_empty() {
        return Err("\"dest\" must not be empty".into());
    }
    let dest = resolve_path(dest_raw, wd);
    if let Ok(m) = fs::symlink_metadata(&dest) {
        if !m.is_dir() {
            return Err(format!("{} is not a directory", dest.display()));
        }
        let occupied = fs::read_dir(&dest).map(|mut r| r.next().is_some()).unwrap_or(true);
        if occupied && !args.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
            return Err(format!(
                "{} is not empty; pass overwrite=true to extract into it",
                dest.display()
            ));
        }
    }
    Ok(ExtractPlan { format, archive, dest })
}

fn list_argv(format: &str, archive: &Path) -> Vec<String> {
    let a = archive.display().to_string();
    match format {
        "zip" => vec!["-Z1".into(), a],
        "tar.gz" => vec!["-tzf".into(), a],
        _ => vec!["l".into(), "-slt".into(), "-bd".into(), "--".into(), a],
    }
}

fn extract_argv(format: &str, archive: &Path, dest: &Path, overwrite: bool) -> Vec<String> {
    let a = archive.display().to_string();
    let d = dest.display().to_string();
    match format {
        "zip" => vec!["-q".into(), if overwrite { "-o" } else { "-n" }.into(), a, "-d".into(), d],
        "tar.gz" => vec!["-xzf".into(), a, "-C".into(), d],
        _ => vec!["x".into(), "-y".into(), "-bd".into(), format!("-o{d}"), a],
    }
}

// Member names per format: zip/tar print one per line; 7z -slt prints
// "Path = " blocks after the dashed separator (the first Path line names
// the archive itself and must be skipped).
fn parse_entries(format: &str, stdout: &str) -> Vec<String> {
    if format != "7z" {
        return stdout.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    }
    let mut out = Vec::new();
    let mut in_listing = false;
    for line in stdout.lines() {
        let line = line.trim_end();
        if !in_listing {
            // shortcut: the archive header block is separated from the
            // entries by a 10-dash rule; upgrade if another 7z build changes it.
            if line.len() >= 10 && line.chars().all(|c| c == '-') {
                in_listing = true;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Path = ") {
            out.push(rest.to_string());
        }
    }
    out
}

// zip-slip guard: no absolute members, no ".." traversal.
fn unsafe_entry(name: &str) -> Option<&'static str> {
    let clean = name.trim_end_matches('/');
    if clean.is_empty() {
        return Some("empty name");
    }
    if clean.starts_with('/') {
        return Some("absolute path");
    }
    if Path::new(clean)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Some("path traversal");
    }
    None
}

fn count_files(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return 1;
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() && !p.is_symlink() {
                    stack.push(p);
                } else {
                    total += 1;
                }
            }
        }
    }
    total
}

pub struct ArchiveTool;

impl Tool for ArchiveTool {
    fn name(&self) -> &str {
        "archive"
    }
    fn description(&self) -> &str {
        "Create or extract zip/tar.gz/7z archives via the system zip/unzip/tar/7z CLIs (argv only, no shell).\naction=create: format zip|tar.gz|7z, sources (files/folders that must exist), dest ending with .zip/.tar.gz/.7z; members are stored under their own names. action=extract: sources is the one archive, dest is the extract folder (created if missing); members with absolute or \"..\" paths are refused, a non-empty dest needs overwrite=true. 7z needs `brew install p7zip`. create without overwrite skips confirmation, extract and overwrite confirm. Returns {ok, path, format, files, bytes} for create, {ok, path, dest, format, entries} for extract."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["create", "extract"] },
                "format": { "type": "string", "enum": ["zip", "tar.gz", "7z"] },
                "sources": { "type": "array", "items": { "type": "string" } },
                "dest": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["action", "sources", "dest"],
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
        if args.get("action").and_then(Value::as_str) == Some("extract") {
            return Some("распаковка архива: файлы будут записаны в папку назначения".to_string());
        }
        args.get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(|| "перезапись существующего архива (overwrite=true)".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
        match action {
            "create" => {
                let plan = match parse_create(args, &wd) {
                    Ok(p) => p,
                    Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
                };
                if plan.dest.exists() && !overwrite {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        format!("{} already exists; pass overwrite=true to replace", plan.dest.display()),
                    ));
                }
                if let Some(parent) = plan.dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                // zip/7z append to an existing archive, so a replaced archive
                // must start from scratch.
                if plan.dest.exists() {
                    fs::remove_file(&plan.dest)?;
                }
                let binary = match binary_for(plan.format, false) {
                    Ok(b) => b,
                    Err(r) => return Ok(r),
                };
                for (cwd, argv) in create_invocations(&plan) {
                    let out = match run_cmd(&binary, &argv, cwd.as_deref(), ARCHIVE_TIMEOUT) {
                        Ok(o) => o,
                        Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
                    };
                    if out.code != Some(0) {
                        return Ok(ToolResult::err(
                            ToolErrorCode::ExecutionError,
                            format!(
                                "{} failed (exit {}): {}",
                                plan.format,
                                out.code.unwrap_or(-1),
                                out.stderr.trim()
                            ),
                        ));
                    }
                }
                let bytes = match fs::metadata(&plan.dest) {
                    Ok(m) if m.is_file() && m.len() > 0 => m.len(),
                    _ => {
                        return Ok(ToolResult::err(
                            ToolErrorCode::ExecutionError,
                            format!("{} exited 0 but left no archive at {}", plan.format, plan.dest.display()),
                        ))
                    }
                };
                let files: u64 = plan.sources.iter().map(|s| count_files(s)).sum();
                Ok(ToolResult::ok(json!({
                    "ok": true,
                    "path": plan.dest.display().to_string(),
                    "format": plan.format,
                    "files": files,
                    "bytes": bytes,
                })))
            }
            "extract" => {
                let plan = match parse_extract(args, &wd) {
                    Ok(p) => p,
                    Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
                };
                let binary = match binary_for(plan.format, true) {
                    Ok(b) => b,
                    Err(r) => return Ok(r),
                };
                Ok(extract_body(&plan, overwrite, &binary).capped(self.max_result_len()))
            }
            other => Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("unknown action \"{other}\" (create|extract)"),
            )),
        }
    }
}

// List members, refuse zip-slip, then extract; takes the resolved binary so
// tests can point it at a fake CLI.
fn extract_body(plan: &ExtractPlan, overwrite: bool, binary: &Path) -> ToolResult {
    let listed = match run_cmd(binary, &list_argv(plan.format, &plan.archive), None, ARCHIVE_TIMEOUT) {
        Ok(o) => o,
        Err(e) => return ToolResult::err(ToolErrorCode::ExecutionError, e),
    };
    if listed.code != Some(0) {
        return ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!(
                "cannot list {}: (exit {}) {}",
                plan.archive.display(),
                listed.code.unwrap_or(-1),
                listed.stderr.trim()
            ),
        );
    }
    let entries = parse_entries(plan.format, &listed.stdout);
    if entries.is_empty() {
        return ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!("{} lists no members", plan.archive.display()),
        );
    }
    if entries.len() > MAX_LIST_ENTRIES {
        return ToolResult::err(
            ToolErrorCode::LimitExceeded,
            format!("archive has more than {MAX_LIST_ENTRIES} members; refusing to extract"),
        );
    }
    for entry in &entries {
        if let Some(reason) = unsafe_entry(entry) {
            return ToolResult::err(
                ToolErrorCode::PermissionDenied,
                format!("archive member \"{entry}\" is unsafe ({reason}); refusing to extract"),
            );
        }
    }
    if let Err(e) = fs::create_dir_all(&plan.dest) {
        return ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!("cannot create {}: {e}", plan.dest.display()),
        );
    }
    let out = match run_cmd(
        binary,
        &extract_argv(plan.format, &plan.archive, &plan.dest, overwrite),
        None,
        ARCHIVE_TIMEOUT,
    ) {
        Ok(o) => o,
        Err(e) => return ToolResult::err(ToolErrorCode::ExecutionError, e),
    };
    if out.code != Some(0) {
        return ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!("extract failed (exit {}): {}", out.code.unwrap_or(-1), out.stderr.trim()),
        );
    }
    ToolResult::ok(json!({
        "ok": true,
        "path": plan.archive.display().to_string(),
        "dest": plan.dest.display().to_string(),
        "format": plan.format,
        "entries": entries.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use std::os::unix::fs::PermissionsExt;

    fn ctx_with_wd(wd: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(wd.to_path_buf()),
            ..Default::default()
        }
    }

    fn sample_tree(wd: &Path) {
        fs::create_dir_all(wd.join("src/sub")).unwrap();
        fs::write(wd.join("src/a.txt"), "one").unwrap();
        fs::write(wd.join("src/sub/b.txt"), "two").unwrap();
        fs::write(wd.join("note.md"), "note").unwrap();
    }

    #[test]
    fn schema_shape() {
        let schema = ArchiveTool.parameters();
        assert!(validate_args(&schema, &json!({ "action": "create", "format": "zip", "sources": ["a"], "dest": "o.zip" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "extract", "sources": ["o.zip"], "dest": "out", "overwrite": true })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "explode", "sources": ["a"], "dest": "o.zip" }))
            .unwrap_err()
            .contains("enum"));
        assert!(validate_args(&schema, &json!({ "action": "create", "sources": ["a"], "dest": "o.zip", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
        assert!(validate_args(&schema, &json!({ "action": "create", "dest": "o.zip" }))
            .unwrap_err()
            .contains("sources"));
    }

    #[test]
    fn create_plan_validates_format_sources_and_dest_extension() {
        let wd = temp_test_dir("arc-plan");
        fs::create_dir_all(wd.join("src")).unwrap();
        fs::write(wd.join("src/a.txt"), "x").unwrap();

        let plan = parse_create(
            &json!({ "format": "zip", "sources": ["src", "src/a.txt"], "dest": "Harness/out.zip" }),
            &wd,
        )
        .unwrap();
        assert_eq!(plan.format, "zip");
        assert_eq!(plan.dest, wd.join("Harness/out.zip"));
        assert_eq!(plan.sources, vec![wd.join("src"), wd.join("src/a.txt")]);

        assert!(parse_create(&json!({ "sources": ["src"], "dest": "o.zip" }), &wd)
            .unwrap_err()
            .contains("format"));
        assert!(parse_create(&json!({ "format": "rar", "sources": ["src"], "dest": "o.rar" }), &wd)
            .unwrap_err()
            .contains("zip, tar.gz, 7z"));
        assert!(parse_create(&json!({ "format": "zip", "sources": [], "dest": "o.zip" }), &wd)
            .unwrap_err()
            .contains("at least one"));
        assert!(parse_create(&json!({ "format": "zip", "sources": ["/definitely/missing-xyz"], "dest": "o.zip" }), &wd)
            .unwrap_err()
            .contains("source not found"));
        assert!(parse_create(&json!({ "format": "zip", "sources": ["src"], "dest": "o.tar.gz" }), &wd)
            .unwrap_err()
            .contains("must end with .zip"));
        assert!(parse_create(&json!({ "format": "tar.gz", "sources": ["src"], "dest": "o.tgz" }), &wd)
            .unwrap_err()
            .contains("must end with .tar.gz"));
        assert!(parse_create(&json!({ "format": "7z", "sources": ["src"], "dest": "o.zip" }), &wd)
            .unwrap_err()
            .contains("must end with .7z"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_invocations_pin_cwd_and_use_plain_argv() {
        let wd = PathBuf::from("/wd");
        let plan = CreatePlan {
            format: "zip",
            dest: wd.join("Harness/out.zip"),
            sources: vec![wd.join("src"), PathBuf::from("/elsewhere/note.md")],
        };
        let inv = create_invocations(&plan);
        assert_eq!(inv.len(), 2);
        assert_eq!(inv[0].0, Some(wd.clone()));
        assert_eq!(
            inv[0].1,
            ["-q", "-r", "/wd/Harness/out.zip", "src"].map(str::to_string).to_vec()
        );
        assert_eq!(inv[1].0, Some(PathBuf::from("/elsewhere")));
        assert_eq!(inv[1].1, ["-q", "-r", "/wd/Harness/out.zip", "note.md"].map(str::to_string).to_vec());

        let plan = CreatePlan {
            format: "7z",
            dest: PathBuf::from("/wd/o.7z"),
            sources: vec![wd.join("src")],
        };
        assert_eq!(create_invocations(&plan)[0].1, ["a", "-bd", "/wd/o.7z", "src"].map(str::to_string).to_vec());

        let plan = CreatePlan {
            format: "tar.gz",
            dest: PathBuf::from("/wd/o.tar.gz"),
            sources: vec![wd.join("src"), PathBuf::from("/elsewhere/note.md")],
        };
        let inv = create_invocations(&plan);
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].0, None);
        assert_eq!(
            inv[0].1,
            [
                "-czf", "/wd/o.tar.gz",
                "-C", "/wd", "src",
                "-C", "/elsewhere", "note.md",
            ]
            .map(str::to_string)
            .to_vec()
        );
    }

    #[test]
    fn extract_plan_detects_format_and_guards_the_dest_folder() {
        let wd = temp_test_dir("arc-extract-plan");
        fs::write(wd.join("o.zip"), "x").unwrap();
        fs::create_dir_all(wd.join("out")).unwrap();
        fs::write(wd.join("out/occupied.txt"), "x").unwrap();

        let plan = parse_extract(&json!({ "sources": ["o.zip"], "dest": "fresh" }), &wd).unwrap();
        assert_eq!(plan.format, "zip");
        assert_eq!(plan.archive, wd.join("o.zip"));
        assert_eq!(plan.dest, wd.join("fresh"));

        fs::write(wd.join("o.tar.gz"), "x").unwrap();
        assert_eq!(
            parse_extract(&json!({ "sources": ["o.tar.gz"], "dest": "fresh" }), &wd)
                .unwrap()
                .format,
            "tar.gz"
        );
        fs::write(wd.join("o.tgz"), "x").unwrap();
        assert_eq!(
            parse_extract(&json!({ "sources": ["o.tgz"], "dest": "fresh" }), &wd)
                .unwrap()
                .format,
            "tar.gz"
        );
        fs::write(wd.join("plain"), "x").unwrap();
        assert!(parse_extract(&json!({ "sources": ["plain"], "dest": "fresh" }), &wd)
            .unwrap_err()
            .contains("cannot detect"));
        assert_eq!(
            parse_extract(&json!({ "sources": ["plain"], "format": "7z", "dest": "fresh" }), &wd)
                .unwrap()
                .format,
            "7z"
        );

        assert!(parse_extract(&json!({ "sources": ["missing.zip"], "dest": "fresh" }), &wd)
            .unwrap_err()
            .contains("archive not found"));
        assert!(parse_extract(&json!({ "sources": ["o.zip", "o.tgz"], "dest": "fresh" }), &wd)
            .unwrap_err()
            .contains("exactly one"));
        assert!(parse_extract(&json!({ "sources": ["o.zip"], "dest": "out" }), &wd)
            .unwrap_err()
            .contains("not empty; pass overwrite=true"));
        assert!(parse_extract(&json!({ "sources": ["o.zip"], "dest": "out", "overwrite": true }), &wd).is_ok());
        assert!(parse_extract(&json!({ "sources": ["o.zip"], "dest": "o.zip" }), &wd)
            .unwrap_err()
            .contains("is not a directory"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn list_and_extract_argv_are_exact_per_format() {
        let a = PathBuf::from("/wd/o.zip");
        let d = PathBuf::from("/wd/out");
        assert_eq!(list_argv("zip", &a), ["-Z1", "/wd/o.zip"].map(str::to_string).to_vec());
        assert_eq!(list_argv("tar.gz", &PathBuf::from("/wd/o.tar.gz")), ["-tzf", "/wd/o.tar.gz"].map(str::to_string).to_vec());
        assert_eq!(
            list_argv("7z", &PathBuf::from("/wd/o.7z")),
            ["l", "-slt", "-bd", "--", "/wd/o.7z"].map(str::to_string).to_vec()
        );
        assert_eq!(
            extract_argv("zip", &a, &d, false),
            ["-q", "-n", "/wd/o.zip", "-d", "/wd/out"].map(str::to_string).to_vec()
        );
        assert_eq!(
            extract_argv("zip", &a, &d, true),
            ["-q", "-o", "/wd/o.zip", "-d", "/wd/out"].map(str::to_string).to_vec()
        );
        assert_eq!(
            extract_argv("tar.gz", &PathBuf::from("/wd/o.tar.gz"), &d, false),
            ["-xzf", "/wd/o.tar.gz", "-C", "/wd/out"].map(str::to_string).to_vec()
        );
        assert_eq!(
            extract_argv("7z", &PathBuf::from("/wd/o.7z"), &d, true),
            ["x", "-y", "-bd", "-o/wd/out", "/wd/o.7z"].map(str::to_string).to_vec()
        );
    }

    #[test]
    fn parse_entries_handles_zip_tar_and_7z_slt() {
        assert_eq!(parse_entries("zip", "src/\nsrc/a.txt\n"), vec!["src/", "src/a.txt"]);
        assert_eq!(parse_entries("tar.gz", "src/\nsrc/a.txt\n"), vec!["src/", "src/a.txt"]);
        let slt = "Path = /wd/o.7z\nType =\n-----------\nPath = src\nSize = 0\n-----------\nPath = src/a.txt\nSize = 3\n";
        assert_eq!(parse_entries("7z", slt), vec!["src", "src/a.txt"]);
    }

    #[test]
    fn unsafe_entry_rejects_slip_paths_and_accepts_relative_ones() {
        assert_eq!(unsafe_entry("src/a.txt"), None);
        assert_eq!(unsafe_entry("src/"), None);
        assert_eq!(unsafe_entry("./a.txt"), None);
        assert_eq!(unsafe_entry("a/./b/../c.txt"), Some("path traversal"));
        assert_eq!(unsafe_entry("../evil.txt"), Some("path traversal"));
        assert_eq!(unsafe_entry("/etc/passwd"), Some("absolute path"));
        assert_eq!(unsafe_entry("dir/"), None);
        assert!(unsafe_entry("").is_some());
        assert!(unsafe_entry("/").is_some());
    }

    #[test]
    fn live_zip_and_tar_round_trip_keeps_clean_member_names() {
        let wd = temp_test_dir("arc-live");
        sample_tree(&wd);
        let ctx = ctx_with_wd(&wd);

        for (format, dest) in [("zip", "Harness/out.zip"), ("tar.gz", "Harness/out.tar.gz")] {
            let r = ArchiveTool
                .execute(&json!({ "action": "create", "format": format, "sources": ["src", "note.md"], "dest": dest }), &ctx)
                .unwrap();
            assert!(r.ok, "{format}: {:?}", r.error);
            let data = r.data.unwrap();
            assert_eq!(data["ok"], true);
            assert_eq!(data["path"], wd.join(dest).display().to_string());
            assert_eq!(data["format"], format);
            assert_eq!(data["files"], 3, "{format} must count src/a.txt, src/sub/b.txt, note.md");
            assert!(data["bytes"].as_u64().unwrap() > 0);

            let r = ArchiveTool
                .execute(&json!({ "action": "extract", "sources": [dest], "dest": "Harness/extracted" }), &ctx)
                .unwrap();
            assert!(r.ok, "{format}: {:?}", r.error);
            let data = r.data.unwrap();
            assert!(data["entries"].as_u64().unwrap() >= 3);
            // clean members: the source names are the archive roots, no
            // absolute path tree is rebuilt under dest
            assert_eq!(fs::read(wd.join("Harness/extracted/src/a.txt")).unwrap(), b"one");
            assert_eq!(fs::read(wd.join("Harness/extracted/src/sub/b.txt")).unwrap(), b"two");
            assert_eq!(fs::read(wd.join("Harness/extracted/note.md")).unwrap(), b"note");
            fs::remove_dir_all(wd.join("Harness/extracted")).unwrap();
        }

        // overwrite guard on create
        let r = ArchiveTool
            .execute(&json!({ "action": "create", "format": "zip", "sources": ["note.md"], "dest": "Harness/out.zip" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("overwrite=true"), "{}", err.message);
        let r = ArchiveTool
            .execute(&json!({ "action": "create", "format": "zip", "sources": ["note.md"], "dest": "Harness/out.zip", "overwrite": true }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        // replaced, not appended: only note.md is inside now
        let r = ArchiveTool
            .execute(&json!({ "action": "extract", "sources": ["Harness/out.zip"], "dest": "Harness/re" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(wd.join("Harness/re/note.md").is_file());
        assert!(!wd.join("Harness/re/a.txt").exists());

        // non-empty extract dir needs overwrite
        let r = ArchiveTool
            .execute(&json!({ "action": "extract", "sources": ["Harness/out.zip"], "dest": "Harness/re" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("not empty"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn live_7z_round_trip_when_available() {
        if find_binary("7z", &["/opt/homebrew/bin/7z", "/usr/local/bin/7z"]).is_none() {
            eprintln!("no 7z on this machine, skipping live 7z test");
            return;
        }
        let wd = temp_test_dir("arc-live-7z");
        sample_tree(&wd);
        let ctx = ctx_with_wd(&wd);
        let r = ArchiveTool
            .execute(&json!({ "action": "create", "format": "7z", "sources": ["src"], "dest": "Harness/o.7z" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let r = ArchiveTool
            .execute(&json!({ "action": "extract", "sources": ["Harness/o.7z"], "dest": "Harness/x" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read(wd.join("Harness/x/src/a.txt")).unwrap(), b"one");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_7z_reports_an_install_hint() {
        // The lookup only fails when 7z is truly absent; on machines with it
        // this asserts nothing, so the hint text is checked via binary_for.
        if find_binary("7z", &["/opt/homebrew/bin/7z", "/usr/local/bin/7z"]).is_some() {
            return;
        }
        let err = binary_for("7z", false).unwrap_err();
        assert_eq!(err.error.unwrap().code, "not_found");
    }

    #[test]
    fn extract_refuses_a_non_archive_and_reports_policies() {
        let wd = temp_test_dir("arc-notarchive");
        fs::write(wd.join("junk.zip"), "not really a zip").unwrap();
        let ctx = ctx_with_wd(&wd);
        let r = ArchiveTool
            .execute(&json!({ "action": "extract", "sources": ["junk.zip"], "dest": "out" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("cannot list"), "{}", err.message);

        assert_eq!(ArchiveTool.class(), ToolClass::Mutating);
        assert_eq!(ArchiveTool.policy(), PermissionPolicy::CheckConditional);
        let create = json!({ "action": "create", "format": "zip", "sources": ["junk.zip"], "dest": "o.zip" });
        assert_eq!(ArchiveTool.danger_reason(&create, &ctx), None);
        let forced = json!({ "action": "create", "format": "zip", "sources": ["junk.zip"], "dest": "o.zip", "overwrite": true });
        assert!(ArchiveTool.danger_reason(&forced, &ctx).is_some());
        let extract = json!({ "action": "extract", "sources": ["junk.zip"], "dest": "out" });
        assert!(ArchiveTool.danger_reason(&extract, &ctx).is_some());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fake_zip_with_traversal_member_is_refused_before_extraction() {
        // A fake unzip that lists a slipping member: extract_body must stop
        // before the second (extract) invocation ever runs.
        let dir = temp_test_dir("arc-slip");
        let wd = temp_test_dir("arc-slip-wd");
        fs::write(wd.join("evil.zip"), "x").unwrap();
        let marker = dir.join("extracted");
        let fake = dir.join("unzip");
        fs::write(
            &fake,
            &format!(
                "#!/bin/sh\ncase \"$1\" in -Z1) printf '../evil.txt\\n';; *) touch '{}'; exit 9;; esac\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let plan = ExtractPlan {
            format: "zip",
            archive: wd.join("evil.zip"),
            dest: wd.join("out"),
        };
        let r = extract_body(&plan, false, &fake);
        let err = r.error.unwrap();
        assert_eq!(err.code, "permission_denied");
        assert!(err.message.contains("path traversal"), "{}", err.message);
        assert!(!wd.join("out").exists());
        assert!(!marker.exists(), "the extract command must never run");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&wd).unwrap();
    }
}
