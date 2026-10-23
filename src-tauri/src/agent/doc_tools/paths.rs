// Shared path resolution and atomic write helpers used by both document tools.
// Project mode keeps every target inside the project root; harness mode accepts
// absolute/~/working-dir-relative paths. Directory targets and existing files
// are refused unless overwrite=true; writes go through a temp file + rename.
use super::super::harness_tools::{canonicalish, resolve_path};
use super::super::project_tools::with_conn;
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::ToolCtx;
use crate::error::{AppError, Result};
use crate::fs_ops;
use crate::projects;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const MAX_CONTENT_BYTES: usize = 2 * 1024 * 1024;

// (label shown back to the model, absolute target path).
pub(super) fn resolve_target(args: &Value, ctx: &ToolCtx, wd: &Path) -> Result<(String, PathBuf)> {
    let raw = args["path"].as_str().unwrap_or_default();
    if raw.trim().is_empty() {
        return Err(AppError::Invalid("path is empty".into()));
    }
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            let joined = crate::path_policy::join_rel(&root, raw)?;
            let name = joined
                .file_name()
                .ok_or_else(|| AppError::Invalid(format!("invalid path \"{raw}\"")))?;
            fs_ops::check_name(&name.to_string_lossy())?;
            // An existing target (also via a lenient dash/space/case/NFD
            // variant) keeps its real path; a new one keeps the requested
            // name under leniently resolved parent folders.
            let joined = match super::super::harness_tools::lenient_existing(&joined) {
                Ok(p) => p,
                Err(e) if e.to_string().contains("ambiguous") => return Err(e),
                Err(_) => crate::path_policy::lenient_parent(&joined)?,
            };
            if !canonicalish(&joined).starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok((raw.to_owned(), joined))
        }
        None => {
            let path = resolve_path(raw, wd);
            let name = path
                .file_name()
                .ok_or_else(|| AppError::Invalid(format!("invalid path \"{raw}\"")))?;
            fs_ops::check_name(&name.to_string_lossy())?;
            let path = match super::super::harness_tools::lenient_existing(&path) {
                Ok(p) => p,
                Err(e) if e.to_string().contains("ambiguous") => return Err(e),
                Err(_) => crate::path_policy::lenient_parent(&path)?,
            };
            Ok((path.display().to_string(), path))
        }
    }
}

pub(super) fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

// Directory targets and existing files are refused unless overwrite=true.
pub(super) fn guard_target(path: &Path, args: &Value) -> std::result::Result<(), ToolResult> {
    if path.is_dir() {
        return Err(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!("{} is a directory", path.display()),
        ));
    }
    if path.exists() && !args.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
        return Err(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!(
                "{} already exists; pass overwrite=true to replace it",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// Parent folders are created on demand; the file itself is written through a
// temp file + rename so a failure never leaves a half-written document.
pub(super) fn write_atomic(path: &Path, content: &[u8]) -> Result<usize> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_owned());
    let tmp = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.doc-tmp-{}", nanos()));
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(content.len())
}

pub(super) fn content_cap_error(len: usize) -> ToolResult {
    ToolResult::err(
        ToolErrorCode::InvalidArgs,
        format!("content is {len} bytes, over the {MAX_CONTENT_BYTES}-byte cap"),
    )
}
