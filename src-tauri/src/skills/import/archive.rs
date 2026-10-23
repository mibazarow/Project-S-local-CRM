// Archive/source extraction for imports: safe .zip extraction and the
// system-git clone seam. Both land their output in a staging folder that the
// import flow cleans up.

use crate::error::{AppError, Result};
use crate::paths;
use std::fs;
use std::path::Path;
use std::process::Command;

fn zip_err(e: zip::result::ZipError) -> AppError {
    AppError::Io(format!("zip: {e}"))
}

// Extract a .zip into `dest`, rejecting unsafe entries: `enclosed_name()`
// drops anything escaping via parent dirs (returns None); raw absolute
// paths are rejected outright instead of being silently stripped.
pub(super) fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_err)?;
        let raw = entry.name().to_string();
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err(AppError::Invalid(format!("zip entry has an absolute path: {raw:?}")));
        }
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| AppError::Invalid(format!("zip entry has an unsafe path: {raw:?}")))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out_file = fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut out_file)?;
        }
    }
    Ok(())
}

// Git import shells out to the system git CLI (no git crate is pulled in);
// TMPDIR/TEMP/TMP point at Harness/tmp/ like the bash tool (SPEC 5.2).
// `exe` is a seam so the missing-git error path is testable.
pub(super) fn run_git(wd: &Path, exe: &str, args: &[&str]) -> Result<()> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let out = Command::new(exe)
        .args(args)
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AppError::Invalid(format!(
                "git executable \"{exe}\" not found: import from git needs the system git CLI"
            )),
            _ => AppError::Io(format!("git spawn: {e}")),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(AppError::Invalid(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}
