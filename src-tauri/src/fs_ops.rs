use crate::error::{AppError, Result};
use crate::paths;
use crate::projects;
use base64::Engine as _;
use rusqlite::Connection;
use serde::Serialize;
use std::fs;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

const MARKDOWN_EXTS: &[&str] = &["md", "markdown"];
pub(crate) const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "tiff", "tif", "heic", "heif",
    "avif",
];
const TEXT_EXTS: &[&str] = &[
    "txt", "csv", "tsv", "json", "json5", "yaml", "yml", "toml", "ini", "conf", "cfg", "xml",
    "html", "htm", "css", "js", "jsx", "ts", "tsx", "rs", "py", "rb", "go", "java", "c", "h",
    "cpp", "hpp", "sh", "bash", "zsh", "sql", "log", "bat", "ps1",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    pub name: String,
    pub rel_path: String,
    pub kind: &'static str,
    pub ext: String,
    pub size: u64,
    pub modified: u64,
    pub is_markdown: bool,
    pub is_text: bool,
    pub is_image: bool,
}

pub fn list_dir(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<Vec<FsEntry>> {
    let (root, dir) = existing(conn, wd, project_id, rel_path)?;
    if !dir.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel_path}\" is not a folder")));
    }
    list_dir_at(&root, &dir)
}

// One level of `dir` as FsEntry relative to `root`, skipping macOS noise and
// sorted dirs-first case-insensitive. Shared by project and notes listings.
pub(crate) fn list_dir_at(root: &Path, dir: &Path) -> Result<Vec<FsEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        // macOS folder noise, not user data; everything else stays visible.
        if item.file_name() == ".DS_Store" {
            continue;
        }
        entries.push(entry(root, &item.path())?);
    }
    sort_entries(&mut entries);
    Ok(entries)
}

pub(crate) fn sort_entries(entries: &mut [FsEntry]) {
    entries.sort_by(|a, b| {
        let dir_rank = |e: &FsEntry| if e.kind == "dir" { 0 } else { 1 };
        dir_rank(a)
            .cmp(&dir_rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

pub fn read_text(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<String> {
    let (_, path) = existing(conn, wd, project_id, rel_path)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel_path}\" is a folder")));
    }
    read_text_at(&path, rel_path)
}

// Reads a file as UTF-8 text, rejecting binary content. `label` names the
// file in errors. Shared by project read_text and the notes vault.
pub(crate) fn read_text_at(path: &Path, label: &str) -> Result<String> {
    let bytes = fs::read(path)?;
    if bytes.contains(&0) {
        return Err(AppError::Invalid(format!("\"{label}\" is binary, not text")));
    }
    String::from_utf8(bytes).map_err(|_| AppError::Invalid(format!("\"{label}\" is not valid UTF-8")))
}

// Canonicalized absolute path of an existing project entry (same safe
// resolver as read_text: rejects escapes and symlink escapes). Used by the
// agent read tools to hand the real path to the document extractor.
pub fn resolve_existing(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel_path: &str,
) -> Result<PathBuf> {
    let (_, path) = existing(conn, wd, project_id, rel_path)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel_path}\" is a folder")));
    }
    Ok(path)
}

pub fn write_text(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel_path: &str,
    content: &str,
) -> Result<FsEntry> {
    let (root, path) = existing(conn, wd, project_id, rel_path)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel_path}\" is a folder")));
    }
    fs::write(&path, content)?;
    entry(&root, &path)
}

pub fn create_folder(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    parent_rel: &str,
    name: &str,
) -> Result<FsEntry> {
    let (root, target) = new_target(conn, wd, project_id, parent_rel, name)?;
    fs::create_dir(&target)?;
    entry(&root, &target)
}

pub fn create_file(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    parent_rel: &str,
    name: &str,
) -> Result<FsEntry> {
    let (root, target) = new_target(conn, wd, project_id, parent_rel, name)?;
    fs::write(&target, "")?;
    entry(&root, &target)
}

pub fn rename(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel_path: &str,
    new_name: &str,
) -> Result<FsEntry> {
    let name = check_name(new_name)?;
    let (root, path) = existing_for_mutation(conn, wd, project_id, rel_path)?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Invalid("cannot rename the project folder".into()))?;
    let target = parent.join(&name);
    if target != path {
        if target.exists() {
            return Err(AppError::Invalid(format!("\"{name}\" already exists")));
        }
        fs::rename(&path, &target)?;
    }
    entry(&root, &target)
}

// Copies the entry next to its source as "<base> копия N<ext>" (files) or
// "<name> копия N" (folders), N = 1, 2, …; the merged «Скопировать» action
// (the former «Дублировать»).
pub fn copy_as(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<FsEntry> {
    let (root, path) = existing_for_mutation(conn, wd, project_id, rel_path)?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Invalid("cannot copy the project folder".into()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = parent.join(copy_suffix_name(parent, &name, path.is_dir()));
    if path.is_dir() {
        copy_tree(&path, &target)?;
    } else {
        fs::copy(&path, &target)?;
    }
    entry(&root, &target)
}

// Canonical absolute path of an existing project entry (same containment as
// read_text); used by «Скопировать путь».
pub fn absolute_path(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<String> {
    let (_, path) = existing(conn, wd, project_id, rel_path)?;
    Ok(path.display().to_string())
}

// Reveals the entry in Finder (`open -R`, macOS).
pub fn reveal_in_finder(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<()> {
    let (_, path) = existing(conn, wd, project_id, rel_path)?;
    let status = std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&path)
        .status()
        .map_err(|e| AppError::Io(format!("open -R: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Io(format!("open -R exited with {status}")))
    }
}

// Archive suffixes understood by extract_archive, longest first so ".tar.gz"
// wins over ".gz". The kind picks the extractor: the zip crate or the system
// tar (which transparently unpacks gz/bz2/xz streams).
const ARCHIVE_SUFFIXES: &[(&str, &str)] = &[
    (".tar.gz", "tar"),
    (".tar.bz2", "tar"),
    (".tar.xz", "tar"),
    (".tgz", "tar"),
    (".tbz2", "tar"),
    (".txz", "tar"),
    (".zip", "zip"),
    (".tar", "tar"),
    (".gz", "tar"),
    (".bz2", "tar"),
    (".xz", "tar"),
];

// (kind, stem) for an archive-named path; None for non-archives or when the
// stem would be empty. The stem keeps the original name's case.
fn archive_kind(path: &Path) -> Option<(&'static str, String)> {
    let raw = path.file_name()?.to_string_lossy().into_owned();
    let lower = raw.to_lowercase();
    for (suffix, kind) in ARCHIVE_SUFFIXES {
        if let Some(stem) = lower.strip_suffix(suffix) {
            if stem.is_empty() {
                return None;
            }
            return Some((kind, raw[..stem.len()].to_owned()));
        }
    }
    None
}

// Extracts a project archive into a sibling folder named after the archive
// stem ("foo.zip" → "foo/", collision → unique_name suffix). Extraction runs
// in a Harness/tmp staging folder first so an unsafe archive never lands in
// the project, then the staging is moved into place.
pub fn extract_archive(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<FsEntry> {
    let (root, path) = existing_for_mutation(conn, wd, project_id, rel_path)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel_path}\" is a folder")));
    }
    let (kind, stem) = archive_kind(&path)
        .ok_or_else(|| AppError::Invalid(format!("\"{rel_path}\" is not a supported archive")))?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Invalid("cannot extract the project folder".into()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging = paths::harness_tmp_dir(wd).join(format!("extract-{nanos}"));
    fs::create_dir_all(&staging)?;
    let extracted = match kind {
        "zip" => extract_zip_into(&path, &staging),
        _ => extract_tar_into(&path, &staging),
    };
    if let Err(e) = extracted {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let dest = parent.join(unique_name(parent, &stem, true));
    fs::rename(&staging, &dest)?;
    entry(&root, &dest)
}

// Unpacks a .zip into `dest`, rejecting absolute, parent-escaping and
// symlink entries (stricter than importer::extract_zip_safe, which skips
// symlinks instead of refusing the archive).
fn extract_zip_into(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| AppError::Invalid(format!("not a valid zip: {e}")))?;
    for i in 0..archive.len() {
        let mut item = archive
            .by_index(i)
            .map_err(|e| AppError::Invalid(format!("broken zip entry: {e}")))?;
        let raw = item.name().to_string();
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err(AppError::Invalid(format!("zip entry has an absolute path: {raw:?}")));
        }
        if item.is_symlink() {
            return Err(AppError::Invalid(format!("zip entry is a symlink: {raw:?}")));
        }
        let rel = item
            .enclosed_name()
            .ok_or_else(|| AppError::Invalid(format!("zip entry has an unsafe path: {raw:?}")))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if item.is_dir() {
            fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out_file = fs::File::create(&out)?;
            std::io::copy(&mut item, &mut out_file)?;
        }
    }
    Ok(())
}

// Pre-scans the tar listing and refuses absolute or ".."-escaping entries,
// then extracts with the system tar into `dest`.
fn extract_tar_into(archive_path: &Path, dest: &Path) -> Result<()> {
    let listing = std::process::Command::new("tar")
        .arg("-tf")
        .arg(archive_path)
        .output()
        .map_err(|e| AppError::Io(format!("tar: {e}")))?;
    if !listing.status.success() {
        let name = archive_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Err(AppError::Invalid(format!("\"{name}\" is not a readable tar archive")));
    }
    for line in String::from_utf8_lossy(&listing.stdout).lines() {
        let entry = Path::new(line);
        if entry.is_absolute()
            || entry.components().any(|c| matches!(c, Component::ParentDir))
        {
            return Err(AppError::Invalid(format!(
                "archive entry escapes the target: {line:?}"
            )));
        }
    }
    let out = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive_path)
        .arg("-C")
        .arg(dest)
        .output()
        .map_err(|e| AppError::Io(format!("tar: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Io(format!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

// Moves the entry into Settings/.trash (reversible delete); returns the
// trash row id so callers can emit trash-changed.
pub fn delete(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<i64> {
    let (_, path) = existing_for_mutation(conn, wd, project_id, rel_path)?;
    let kind = if path.is_dir() { "dir" } else { "file" };
    crate::reliability::trash_put(conn, wd, "project", kind, &path, None)
}

pub fn move_entry(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    from_rel: &str,
    to_dir_rel: &str,
) -> Result<FsEntry> {
    let (root, from) = existing_for_mutation(conn, wd, project_id, from_rel)?;
    let (_, dest_dir) = existing(conn, wd, project_id, to_dir_rel)?;
    if !dest_dir.is_dir() {
        return Err(AppError::Invalid(format!(
            "\"{to_dir_rel}\" is not a folder"
        )));
    }
    if from.is_dir() && dest_dir.starts_with(&from) {
        return Err(AppError::Invalid(
            "cannot move a folder into itself or its own subfolder".into(),
        ));
    }
    let name = from
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = dest_dir.join(unique_name(&dest_dir, &name, from.is_dir()));
    fs::rename(&from, &target)?;
    entry(&root, &target)
}

pub fn import(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    dest_rel: &str,
    source_paths: Vec<String>,
) -> Result<Vec<FsEntry>> {
    let (root, dest) = existing(conn, wd, project_id, dest_rel)?;
    if !dest.is_dir() {
        return Err(AppError::Invalid(format!("\"{dest_rel}\" is not a folder")));
    }
    let mut out = Vec::new();
    for src in source_paths {
        let src_path = PathBuf::from(&src);
        if !src_path.is_absolute() {
            return Err(AppError::Invalid(format!(
                "source path \"{src}\" must be absolute"
            )));
        }
        let canon = src_path
            .canonicalize()
            .map_err(|_| AppError::Invalid(format!("source \"{src}\" not found")))?;
        if canon == root || root.starts_with(&canon) {
            return Err(AppError::Invalid(
                "cannot import a project folder into itself".into(),
            ));
        }
        let name = canon
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| AppError::Invalid(format!("source \"{src}\" has no name")))?;
        let target = dest.join(unique_name(&dest, &name, canon.is_dir()));
        if canon.is_dir() {
            copy_tree(&canon, &target)?;
        } else {
            fs::copy(&canon, &target)?;
        }
        out.push(entry(&root, &target)?);
    }
    Ok(out)
}

pub fn open_external(
    app: &tauri::AppHandle,
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel_path: &str,
) -> Result<()> {
    use tauri_plugin_opener::OpenerExt;
    let (_, path) = existing(conn, wd, project_id, rel_path)?;
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(|e| AppError::Io(e.to_string()))
}

// Hard cap for inline PDF previews in the file panel: the backend clamps
// any larger caller max_bytes to this value (fs_read_binary_data_url).
const PDF_MAX_BYTES: u64 = 25 * 1024 * 1024;

// Data URL for the file preview: images (up to max_bytes) and PDFs (up to
// min(max_bytes, PDF_MAX_BYTES) = 25 MB); every other binary is rejected.
pub fn read_binary_data_url(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel_path: &str,
    max_bytes: u64,
) -> Result<String> {
    let (root, path) = existing(conn, wd, project_id, rel_path)?;
    let item = entry(&root, &path)?;
    if item.is_image {
        return binary_data_url(&path, max_bytes, rel_path);
    }
    if item.ext == "pdf" {
        return pdf_data_url(&path, max_bytes, rel_path);
    }
    Err(AppError::Invalid(format!(
        "\"{rel_path}\" is not an image or PDF"
    )))
}

// Reads a PDF into a base64 data URL for the inline preview. The size cap is
// min(max_bytes, PDF_MAX_BYTES); the content must start with the "%PDF-"
// magic so a renamed binary is rejected.
fn pdf_data_url(path: &Path, max_bytes: u64, label: &str) -> Result<String> {
    let cap = max_bytes.min(PDF_MAX_BYTES);
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > cap {
        return Err(AppError::Invalid(format!(
            "pdf is {size} bytes, limit is {cap}",
        )));
    }
    let bytes = fs::read(path)?;
    if !bytes.starts_with(b"%PDF-") {
        return Err(AppError::Invalid(format!("\"{label}\" is not a valid PDF")));
    }
    Ok(format!(
        "data:application/pdf;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

// Reads an image file into a base64 data URL, rejecting non-images and files
// over max_bytes. `label` names the file in errors. Shared by the chat
// attachment previews, which resolve the path themselves (attachments.rs),
// and the LLM image parts (llm/mod.rs).
pub(crate) fn binary_data_url(path: &Path, max_bytes: u64, label: &str) -> Result<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err(AppError::Invalid(format!("\"{label}\" is not an image")));
    }
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > max_bytes {
        return Err(AppError::Invalid(format!(
            "image is {size} bytes, limit is {max_bytes}",
        )));
    }
    let bytes = fs::read(path)?;
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg".into(),
        "svg" => "image/svg+xml".into(),
        other => format!("image/{other}"),
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

// Canonical root plus the canonical target of rel_path inside it, rejecting
// absolute paths, "..", and symlink escapes (resolved target must stay
// inside). `label` names the containing folder in errors ("project", "Notes").
// Shared by the project resolvers below and the notes vault (notes/mod.rs).
// Missing exact names go through the lenient resolver (see lenient_join), so
// NBSP/em-dash/case/NFD variants of an existing entry resolve to the real
// on-disk name instead of failing.
pub(crate) fn contain_existing(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let joined = resolve_rel(root, rel, label)?;
    let canon = joined
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("\"{rel}\" not found")))?;
    if !canon.starts_with(root) {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" escapes the {label} folder"
        )));
    }
    Ok(canon)
}

// ---------------------------------------------------------------------------
// Lenient path resolution (SPEC: unreliable reads of names with NBSP, em
// dash, №, NFD, case variants). Exact names always win; a miss is re-matched
// component by component against the real directory entries.
// ---------------------------------------------------------------------------

// Folds a name for lenient comparison: trims, normalizes to NFC (so NFC and
// NFD forms compare equal), lowercases, maps dash variants (‐ ‒ – — ─ − and
// soft hyphen) to '-', drops zero-width characters and maps every other
// Unicode space (NBSP, thin, narrow, ideographic, …) to a plain space.
pub(crate) fn fold_name(raw: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().nfc() {
        match ch {
            '\u{00ad}' | '\u{2010}'..='\u{2015}' | '\u{2043}' | '\u{2212}' => out.push('-'),
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' => {}
            c if c.is_whitespace() => out.push(' '),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

// Character-level Levenshtein distance, used only to suggest near names.
fn levenshtein(a: &str, b: &str) -> usize {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=bc.len()).collect();
    let mut cur = vec![0usize; bc.len() + 1];
    for i in 1..=ac.len() {
        cur[0] = i;
        for j in 1..=bc.len() {
            let cost = usize::from(ac[i - 1] != bc[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bc.len()]
}

// "Close enough to be worth suggesting" for the not-found hint.
fn is_close(a: &str, b: &str) -> bool {
    let longest = a.chars().count().max(b.chars().count());
    longest > 0 && levenshtein(a, b) <= (longest / 3).max(2)
}

// Resolves one component inside `dir`: the exact name first (symlinks pass
// through; callers canonicalize/refuse later), then a lenient match against
// the real entries. A unique match returns the ACTUAL on-disk path; several
// matches are refused with the candidate list (never guessed); none is an
// error naming the nearest candidates as a hint.
fn resolve_component(dir: &Path, name: &OsStr, rel: &str, label: &str) -> Result<PathBuf> {
    let exact = dir.join(name);
    if fs::symlink_metadata(&exact).is_ok() {
        return Ok(exact);
    }
    let folded = fold_name(&name.to_string_lossy());
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut near: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for item in rd.flatten() {
            let entry_name = item.file_name().to_string_lossy().into_owned();
            let entry_folded = fold_name(&entry_name);
            if entry_folded == folded {
                matches.push(item.path());
            } else if is_close(&entry_folded, &folded) {
                near.push(entry_name);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap_or(exact)),
        0 => {
            near.sort();
            near.truncate(5);
            let hint = if near.is_empty() {
                String::new()
            } else {
                let list = near
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let tool = if label == "project" { "list_tree" } else { "the folder listing" };
                format!("; nearest names here: {list}; use {tool} for exact names")
            };
            Err(AppError::Invalid(format!(
                "\"{rel}\" not found in the {label} folder{hint}"
            )))
        }
        _ => {
            let mut names: Vec<String> = matches
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            names.sort();
            let list = names
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ");
            Err(AppError::Invalid(format!(
                "\"{rel}\" is ambiguous in the {label} folder: {list} all match; use one exact name"
            )))
        }
    }
}

// Lenient per-component resolution of a root-relative path: returns the
// actual on-disk path; exact components pass through untouched.
pub(crate) fn lenient_join(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let mut cur = root.to_path_buf();
    for part in Path::new(rel).components() {
        match part {
            Component::Normal(name) => cur = resolve_component(&cur, name, rel, label)?,
            _ => {
                return Err(AppError::Invalid(format!(
                    "path \"{rel}\" is not project-relative"
                )))
            }
        }
    }
    Ok(cur)
}

// join_rel + lenient fallback: the resolved (not yet canonicalized) target
// of a root-relative path, for callers that must inspect the pre-canonical
// path (symlink refusal) before canonicalizing.
pub(crate) fn resolve_rel(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let joined = join_rel(root, rel)?;
    if fs::symlink_metadata(&joined).is_ok() {
        return Ok(joined);
    }
    lenient_join(root, rel, label)
}

// Lenient per-component resolution of an absolute, lexically normalized
// path (harness tools): every component is matched exactly first, then
// leniently against the real entries of its parent folder.
pub(crate) fn lenient_absolute(path: &Path) -> Result<PathBuf> {
    let mut cur = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::Prefix(_) => cur.push(part.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                cur.pop();
            }
            Component::Normal(name) => {
                cur = resolve_component(&cur, name, &path.display().to_string(), "folder")?
            }
        }
    }
    Ok(cur)
}

// Re-anchors the parent folders of an absolute path leniently, keeping the
// final component exactly as requested (create/write targets whose own name
// may be new). A truly missing parent chain is returned unchanged so the
// caller can create it; an ambiguous parent folder is refused.
pub(crate) fn lenient_parent(path: &Path) -> Result<PathBuf> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(path.to_path_buf());
    };
    let resolved = if fs::symlink_metadata(parent).is_ok() {
        parent.to_path_buf()
    } else {
        match lenient_absolute(parent) {
            Ok(p) => p,
            Err(e) if e.to_string().contains("ambiguous") => return Err(e),
            Err(_) => parent.to_path_buf(),
        }
    };
    Ok(match path.file_name() {
        Some(name) => resolved.join(name),
        None => resolved,
    })
}

// Refuses symlink entries for mutating operations.
pub(crate) fn reject_symlink(joined: &Path, rel: &str) -> Result<()> {
    if fs::symlink_metadata(joined)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" is a symlink; symlink operations are not supported"
        )));
    }
    Ok(())
}

// Refuses a path when any existing component below root is a symlink, so a
// target created through auto-created parents cannot land outside root.
pub(crate) fn reject_symlink_chain(root: &Path, joined: &Path, rel: &str) -> Result<()> {
    let Ok(rest) = joined.strip_prefix(root) else {
        return Ok(());
    };
    let mut path = root.to_path_buf();
    for part in rest.components() {
        path.push(part.as_os_str());
        reject_symlink(&path, rel)?;
    }
    Ok(())
}

fn existing(conn: &Connection, wd: &Path, project_id: i64, rel: &str) -> Result<(PathBuf, PathBuf)> {
    let root = projects::project_root(conn, wd, project_id)?;
    let canon = contain_existing(&root, rel, "project")?;
    Ok((root, canon))
}

// Like existing(), but refuses the project root itself and symlink entries,
// for rename/duplicate/delete/move.
fn existing_for_mutation(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel: &str,
) -> Result<(PathBuf, PathBuf)> {
    if rel.trim().is_empty() {
        return Err(AppError::Invalid(
            "cannot modify the project folder itself".into(),
        ));
    }
    let root = projects::project_root(conn, wd, project_id)?;
    let joined = resolve_rel(&root, rel, "project")?;
    reject_symlink(&joined, rel)?;
    let canon = joined
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("\"{rel}\" not found")))?;
    if !canon.starts_with(&root) {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" escapes the project folder"
        )));
    }
    Ok((root, canon))
}

fn new_target(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    parent_rel: &str,
    name: &str,
) -> Result<(PathBuf, PathBuf)> {
    let name = check_name(name)?;
    let (root, parent) = existing(conn, wd, project_id, parent_rel)?;
    if !parent.is_dir() {
        return Err(AppError::Invalid(format!(
            "\"{parent_rel}\" is not a folder"
        )));
    }
    let target = parent.join(&name);
    if target.exists() {
        return Err(AppError::Invalid(format!("\"{name}\" already exists")));
    }
    Ok((root, target))
}

pub(crate) fn join_rel(root: &Path, rel: &str) -> Result<PathBuf> {
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

pub(crate) fn check_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(AppError::Invalid(format!("invalid name \"{raw}\"")));
    }
    Ok(name.to_owned())
}

pub(crate) fn entry(root: &Path, path: &Path) -> Result<FsEntry> {
    let meta = fs::metadata(path)?;
    let is_dir = meta.is_dir();
    let ext = if is_dir {
        String::new()
    } else {
        path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let is_markdown = MARKDOWN_EXTS.contains(&ext.as_str());
    Ok(FsEntry {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        rel_path: path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kind: if is_dir { "dir" } else { "file" },
        size: if is_dir { 0 } else { meta.len() },
        modified: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        is_markdown,
        is_text: !is_dir && (is_markdown || TEXT_EXTS.contains(&ext.as_str())),
        is_image: !is_dir && IMAGE_EXTS.contains(&ext.as_str()),
        ext,
    })
}

pub(crate) fn unique_name(parent: &Path, name: &str, is_dir: bool) -> String {
    if !parent.join(name).exists() {
        return name.to_owned();
    }
    let (stem, ext) = split_stem(name, is_dir);
    let mut i = 0;
    loop {
        let candidate = if i == 0 {
            format!("{stem} copy{ext}")
        } else {
            format!("{stem} copy {i}{ext}")
        };
        if !parent.join(&candidate).exists() {
            return candidate;
        }
        i += 1;
    }
}

// macOS-style copy suffix, always present: "<stem> копия N<ext>", N = 1, 2, …
pub(crate) fn copy_suffix_name(parent: &Path, name: &str, is_dir: bool) -> String {
    let (stem, ext) = split_stem(name, is_dir);
    let mut i = 1;
    loop {
        let candidate = format!("{stem} копия {i}{ext}");
        if !parent.join(&candidate).exists() {
            return candidate;
        }
        i += 1;
    }
}

fn split_stem(name: &str, is_dir: bool) -> (String, String) {
    if is_dir {
        return (name.to_owned(), String::new());
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_owned(), name[i..].to_owned()),
        _ => (name.to_owned(), String::new()),
    }
}

pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for item in fs::read_dir(&src_dir)? {
            let item = item?;
            let dst = dst_dir.join(item.file_name());
            if item.file_type()?.is_dir() {
                fs::create_dir_all(&dst)?;
                stack.push((item.path(), dst));
            } else {
                fs::copy(item.path(), &dst)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use crate::paths;

    fn setup(tag: &str) -> (PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        (wd, conn, dto.id)
    }

    fn root(wd: &Path) -> PathBuf {
        paths::projects_dir(wd).join("proj")
    }

    fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write as _;
        use zip::write::FileOptions;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        for (name, content) in entries {
            w.start_file(name, opts).unwrap();
            w.write_all(content.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    // Minimal ustar archive built by hand, so a test can carry a "../" entry
    // that the system tar refuses to create.
    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            let mut h = vec![0u8; 512];
            let set = |h: &mut Vec<u8>, off: usize, val: &str| {
                h[off..off + val.len()].copy_from_slice(val.as_bytes());
            };
            set(&mut h, 0, name);
            set(&mut h, 100, "0000644\0");
            set(&mut h, 108, "0000000\0");
            set(&mut h, 116, "0000000\0");
            set(&mut h, 124, &format!("{:011o}\0", data.len()));
            set(&mut h, 136, "00000000000\0");
            for b in h[148..156].iter_mut() {
                *b = b' ';
            }
            h[156] = b'0';
            set(&mut h, 257, "ustar\0");
            set(&mut h, 263, "00");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            set(&mut h, 148, &format!("{sum:06o}\0 "));
            out.extend_from_slice(&h);
            out.extend_from_slice(data);
            out.resize(out.len() + (512 - data.len() % 512) % 512, 0);
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    #[test]
    fn fold_name_unifies_normalization_case_dashes_and_spaces() {
        // NFD (е + combining diaeresis) and NFC (ё) of the same name compare equal
        assert_eq!(fold_name("Все\u{308} ёлки"), fold_name("Всё ёлки"));
        // every dash variant folds to a plain hyphen
        for dash in [
            '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
        ] {
            assert_eq!(fold_name(&format!("a{dash}b")), "a-b", "dash {dash:?}");
        }
        // NBSP and thin space each fold to a regular space, zero-width dropped
        assert_eq!(fold_name("a\u{a0}\u{2009}b"), "a  b");
        assert_eq!(fold_name("a\u{200b}b"), "ab");
        // case folding and trimming
        assert_eq!(fold_name("  ДОГОВОР №  "), fold_name("договор №"));
    }

    #[test]
    fn lenient_reads_resolve_special_character_name_variants() {
        let (wd, conn, id) = setup("fs-lenient");
        create_folder(&conn, &wd, id, "", "02_Договоры").unwrap();
        // the real demo shape: №, hyphen in the number, NBSP before an em dash
        let real = "Договор № 017-2026\u{a0}— копия.docx";
        fs::write(root(&wd).join(format!("02_Договоры/{real}")), "doc bytes").unwrap();
        fs::write(
            root(&wd).join("Отчёт – 2026 (финал)! #1 [50%] \"цитата\"?.md"),
            "p1",
        )
        .unwrap();
        fs::write(root(&wd).join("mixed CASE.txt"), "p2").unwrap();
        fs::write(root(&wd).join("Все\u{308} ёлки.md"), "p3").unwrap(); // NFD on disk

        // exact name still wins
        assert_eq!(
            read_text(&conn, &wd, id, &format!("02_Договоры/{real}")).unwrap(),
            "doc bytes"
        );
        // hyphen + regular spaces (what the agent typed) resolve to the real file
        assert_eq!(
            read_text(&conn, &wd, id, "02_Договоры/Договор № 017-2026 - копия.docx").unwrap(),
            "doc bytes"
        );
        // en dash variant likewise
        assert_eq!(
            read_text(&conn, &wd, id, "02_Договоры/Договор № 017-2026 – копия.docx").unwrap(),
            "doc bytes"
        );
        // the folder itself via a case variant
        assert_eq!(
            read_text(&conn, &wd, id, "02_договоры/Договор № 017-2026 - копия.docx").unwrap(),
            "doc bytes"
        );
        // punctuation-heavy name: exact and case/dash variants
        let punct = "Отчёт – 2026 (финал)! #1 [50%] \"цитата\"?.md";
        assert_eq!(read_text(&conn, &wd, id, punct).unwrap(), "p1");
        assert_eq!(
            read_text(&conn, &wd, id, "отчёт - 2026 (Финал)! #1 [50%] \"Цитата\"?.md").unwrap(),
            "p1"
        );
        // case-insensitive lookup
        assert_eq!(read_text(&conn, &wd, id, "MIXED case.TXT").unwrap(), "p2");
        // NFC query against an NFD name on disk
        assert_eq!(read_text(&conn, &wd, id, "Всё ёлки.md").unwrap(), "p3");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn lenient_ambiguous_match_lists_candidates_without_guessing() {
        let (wd, conn, id) = setup("fs-ambiguous");
        fs::write(root(&wd).join("a\u{2014}b.txt"), "em").unwrap();
        fs::write(root(&wd).join("a\u{2013}b.txt"), "en").unwrap();
        let msg = read_text(&conn, &wd, id, "a-b.txt").unwrap_err().to_string();
        assert!(msg.contains("ambiguous"), "{msg}");
        assert!(msg.contains("a\u{2014}b.txt") && msg.contains("a\u{2013}b.txt"), "{msg}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn lenient_not_found_error_hints_near_candidates() {
        let (wd, conn, id) = setup("fs-near");
        fs::write(root(&wd).join("Договор № 017-2026.docx"), "x").unwrap();
        let msg = read_text(&conn, &wd, id, "Договор № 017-2027.docx")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("Договор № 017-2026.docx"), "{msg}");
        assert!(msg.contains("list_tree"), "{msg}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn lenient_mutation_ops_use_the_real_on_disk_name() {
        let (wd, conn, id) = setup("fs-lenient-mutate");
        let real = "Заметка\u{a0}— правка.md";
        fs::write(root(&wd).join(real), "one").unwrap();
        // rename via a variant name renames the real file
        let renamed = rename(&conn, &wd, id, "Заметка - правка.md", "done.md").unwrap();
        assert_eq!(renamed.rel_path, "done.md");
        assert!(!root(&wd).join(real).exists());
        // delete via a variant name trashes the real file
        fs::write(root(&wd).join(real), "two").unwrap();
        delete(&conn, &wd, id, "Заметка – правка.md").unwrap();
        assert!(!root(&wd).join(real).exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn lenient_resolution_still_rejects_traversal() {
        let (wd, conn, id) = setup("fs-lenient-escape");
        create_folder(&conn, &wd, id, "", "02_Договоры").unwrap();
        for bad in [
            "02_договоры/../../etc/passwd",
            "..",
            "../02_договоры",
            "02_договоры/./x",
        ] {
            assert!(
                matches!(read_text(&conn, &wd, id, bad), Err(AppError::Invalid(_))),
                "accepted: {bad:?}"
            );
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn path_escapes_are_rejected() {
        let (wd, conn, id) = setup("fs-escape");
        for bad in ["../x", "..", "/etc/passwd", "a/../../x", "./x", "a/./b"] {
            assert!(
                matches!(list_dir(&conn, &wd, id, bad), Err(AppError::Invalid(_))),
                "accepted: {bad:?}"
            );
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_are_rejected() {
        let (wd, conn, id) = setup("fs-symlink");
        std::os::unix::fs::symlink("/etc", root(&wd).join("dirlink")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", root(&wd).join("filelink")).unwrap();
        assert!(matches!(
            list_dir(&conn, &wd, id, "dirlink"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            read_text(&conn, &wd, id, "filelink"),
            Err(AppError::Invalid(_))
        ));
        // even an in-project symlink is refused for mutating ops
        fs::write(root(&wd).join("a.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root(&wd).join("a.txt"), root(&wd).join("link.txt")).unwrap();
        assert!(matches!(
            delete(&conn, &wd, id, "link.txt"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_and_list_sorts_dirs_first_case_insensitive() {
        let (wd, conn, id) = setup("fs-list");
        create_folder(&conn, &wd, id, "", "Beta").unwrap();
        create_folder(&conn, &wd, id, "", "alpha").unwrap();
        create_file(&conn, &wd, id, "", "B.txt").unwrap();
        create_file(&conn, &wd, id, "", "a.md").unwrap();
        fs::write(root(&wd).join(".DS_Store"), "junk").unwrap();
        let items = list_dir(&conn, &wd, id, "").unwrap();
        let names: Vec<&str> = items.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "tasks", "Входящие", "a.md", "B.txt", "datas.md"]);
        let md = items.iter().find(|e| e.name == "a.md").unwrap();
        assert!(md.is_markdown && md.is_text && !md.is_image);
        assert_eq!(md.rel_path, "a.md");
        assert_eq!(items[0].kind, "dir");
        assert_eq!(items[0].rel_path, "alpha");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_rejects_bad_names_and_existing_targets() {
        let (wd, conn, id) = setup("fs-names");
        assert!(matches!(
            create_file(&conn, &wd, id, "", "a/b"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            create_file(&conn, &wd, id, "", ".."),
            Err(AppError::Invalid(_))
        ));
        create_file(&conn, &wd, id, "", "x.txt").unwrap();
        assert!(matches!(
            create_file(&conn, &wd, id, "", "x.txt"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn read_write_text_roundtrip_and_binary_rejected() {
        let (wd, conn, id) = setup("fs-text");
        create_file(&conn, &wd, id, "", "note.md").unwrap();
        let saved = write_text(&conn, &wd, id, "note.md", "привет").unwrap();
        assert_eq!(saved.rel_path, "note.md");
        assert_eq!(read_text(&conn, &wd, id, "note.md").unwrap(), "привет");
        fs::write(root(&wd).join("bin.dat"), [1u8, 2, 0, 3]).unwrap();
        assert!(matches!(
            read_text(&conn, &wd, id, "bin.dat"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn rename_copy_as_delete_roundtrip() {
        let (wd, conn, id) = setup("fs-roundtrip");
        create_file(&conn, &wd, id, "", "a.txt").unwrap();
        write_text(&conn, &wd, id, "a.txt", "one").unwrap();
        let renamed = rename(&conn, &wd, id, "a.txt", "b.txt").unwrap();
        assert_eq!(renamed.rel_path, "b.txt");
        assert!(!root(&wd).join("a.txt").exists());
        assert_eq!(read_text(&conn, &wd, id, "b.txt").unwrap(), "one");
        assert!(rename(&conn, &wd, id, "b.txt", "b.txt").is_ok()); // same name is a no-op

        let dup = copy_as(&conn, &wd, id, "b.txt").unwrap();
        assert_eq!(dup.rel_path, "b копия 1.txt");
        assert_eq!(read_text(&conn, &wd, id, "b копия 1.txt").unwrap(), "one");
        let dup2 = copy_as(&conn, &wd, id, "b.txt").unwrap();
        assert_eq!(dup2.rel_path, "b копия 2.txt");

        create_folder(&conn, &wd, id, "", "d").unwrap();
        create_file(&conn, &wd, id, "d", "inner.md").unwrap();
        let dir_dup = copy_as(&conn, &wd, id, "d").unwrap();
        assert_eq!(dir_dup.rel_path, "d копия 1");
        assert!(root(&wd).join("d копия 1/inner.md").is_file());

        delete(&conn, &wd, id, "d").unwrap();
        assert!(!root(&wd).join("d").exists());
        assert!(matches!(
            delete(&conn, &wd, id, ""),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn copy_as_numbers_copies_and_keeps_extension() {
        let (wd, conn, id) = setup("fs-copyas");
        create_file(&conn, &wd, id, "", "report.final.md").unwrap();
        let c1 = copy_as(&conn, &wd, id, "report.final.md").unwrap();
        assert_eq!(c1.rel_path, "report.final копия 1.md");
        let c2 = copy_as(&conn, &wd, id, "report.final.md").unwrap();
        assert_eq!(c2.rel_path, "report.final копия 2.md");
        let c3 = copy_as(&conn, &wd, id, "report.final копия 1.md").unwrap();
        assert_eq!(c3.rel_path, "report.final копия 1 копия 1.md");
        // a dotfile keeps its full name as the stem
        create_file(&conn, &wd, id, "", ".hidden").unwrap();
        let h = copy_as(&conn, &wd, id, ".hidden").unwrap();
        assert_eq!(h.rel_path, ".hidden копия 1");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn absolute_path_is_canonical_and_contained() {
        let (wd, conn, id) = setup("fs-abs");
        create_folder(&conn, &wd, id, "", "sub").unwrap();
        create_file(&conn, &wd, id, "sub", "a.txt").unwrap();
        let abs = absolute_path(&conn, &wd, id, "sub/a.txt").unwrap();
        assert_eq!(
            Path::new(&abs),
            root(&wd).join("sub/a.txt").canonicalize().unwrap()
        );
        for bad in ["../x", "..", "/etc/passwd", "a/../../x", "missing.txt"] {
            assert!(
                matches!(absolute_path(&conn, &wd, id, bad), Err(AppError::Invalid(_))),
                "accepted: {bad:?}"
            );
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn extract_archive_zip_into_sibling_folder_with_collision_suffix() {
        let (wd, conn, id) = setup("fs-extract-zip");
        fs::write(root(&wd).join("pkg.zip"), zip_bytes(&[("inner.txt", "hi"), ("sub/deep.txt", "deep")])).unwrap();
        let out = extract_archive(&conn, &wd, id, "pkg.zip").unwrap();
        assert_eq!(out.rel_path, "pkg");
        assert_eq!(out.kind, "dir");
        assert_eq!(fs::read_to_string(root(&wd).join("pkg/inner.txt")).unwrap(), "hi");
        assert!(root(&wd).join("pkg/sub/deep.txt").is_file());
        // collision → suffixed sibling, the first extraction stays intact
        let again = extract_archive(&conn, &wd, id, "pkg.zip").unwrap();
        assert_eq!(again.rel_path, "pkg copy");
        // not an archive → rejected
        fs::write(root(&wd).join("plain.txt"), "x").unwrap();
        assert!(matches!(
            extract_archive(&conn, &wd, id, "plain.txt"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            extract_archive(&conn, &wd, id, "pkg"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn extract_archive_rejects_unsafe_zip_entries() {
        let (wd, conn, id) = setup("fs-extract-evil-zip");
        fs::write(
            root(&wd).join("evil.zip"),
            zip_bytes(&[("ok.txt", "ok"), ("../escape.txt", "nope")]),
        )
        .unwrap();
        assert!(matches!(
            extract_archive(&conn, &wd, id, "evil.zip"),
            Err(AppError::Invalid(_))
        ));
        assert!(!wd.join("escape.txt").exists());
        assert!(!root(&wd).join("evil").exists());
        // the staging folder is cleaned up after the refusal
        let staging = paths::harness_tmp_dir(&wd);
        let leftovers = fs::read_dir(&staging).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn extract_archive_tar_gz_roundtrip() {
        let (wd, conn, id) = setup("fs-extract-targz");
        let src = temp_test_dir("fs-extract-targz-src");
        fs::create_dir_all(src.join("docs")).unwrap();
        fs::write(src.join("docs/hello.txt"), "hi").unwrap();
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(root(&wd).join("docs.tar.gz"))
            .arg("-C")
            .arg(&src)
            .arg("docs")
            .status()
            .unwrap();
        assert!(status.success());
        let out = extract_archive(&conn, &wd, id, "docs.tar.gz").unwrap();
        assert_eq!(out.rel_path, "docs");
        // the archive carries a top-level "docs/" entry, contents land inside
        // the stem folder as-is
        assert_eq!(
            fs::read_to_string(root(&wd).join("docs/docs/hello.txt")).unwrap(),
            "hi"
        );
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn extract_archive_rejects_escaping_tar_entries() {
        let (wd, conn, id) = setup("fs-extract-evil-tar");
        fs::write(
            root(&wd).join("evil.tar"),
            tar_bytes(&[("../escape.txt", b"nope".as_slice())]),
        )
        .unwrap();
        assert!(matches!(
            extract_archive(&conn, &wd, id, "evil.tar"),
            Err(AppError::Invalid(_))
        ));
        assert!(!wd.join("escape.txt").exists());
        assert!(!root(&wd).join("evil").exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn move_roundtrip_rejects_into_descendant_and_renames_on_collision() {
        let (wd, conn, id) = setup("fs-move");
        create_folder(&conn, &wd, id, "", "src").unwrap();
        create_folder(&conn, &wd, id, "", "dst").unwrap();
        create_file(&conn, &wd, id, "src", "f.txt").unwrap();
        let moved = move_entry(&conn, &wd, id, "src/f.txt", "dst").unwrap();
        assert_eq!(moved.rel_path, "dst/f.txt");

        create_folder(&conn, &wd, id, "dst", "deep").unwrap();
        assert!(matches!(
            move_entry(&conn, &wd, id, "dst", "dst/deep"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            move_entry(&conn, &wd, id, "dst", "dst"),
            Err(AppError::Invalid(_))
        ));

        create_file(&conn, &wd, id, "src", "f.txt").unwrap();
        let collided = move_entry(&conn, &wd, id, "src/f.txt", "dst").unwrap();
        assert_eq!(collided.rel_path, "dst/f copy.txt");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn import_copies_files_and_folders_with_collision_handling() {
        let (wd, conn, id) = setup("fs-import");
        let outside = temp_test_dir("fs-import-src");
        fs::write(outside.join("photo.png"), [1u8, 2, 3]).unwrap();
        fs::create_dir_all(outside.join("docs/sub")).unwrap();
        fs::write(outside.join("docs/sub/x.txt"), "y").unwrap();

        let items = import(
            &conn,
            &wd,
            id,
            "",
            vec![
                outside.join("photo.png").display().to_string(),
                outside.join("docs").display().to_string(),
            ],
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_image);
        assert!(root(&wd).join("photo.png").is_file());
        assert!(root(&wd).join("docs/sub/x.txt").is_file());

        let again = import(
            &conn,
            &wd,
            id,
            "",
            vec![outside.join("photo.png").display().to_string()],
        )
        .unwrap();
        assert_eq!(again[0].rel_path, "photo copy.png");

        // refusing to import the project into itself
        assert!(matches!(
            import(&conn, &wd, id, "", vec![root(&wd).display().to_string()]),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            import(&conn, &wd, id, "", vec!["photo.png".into()]),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn image_data_url_respects_kind_and_limit() {
        let (wd, conn, id) = setup("fs-dataurl");
        fs::write(root(&wd).join("pic.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
        fs::write(root(&wd).join("doc.txt"), "text").unwrap();
        let url = read_binary_data_url(&conn, &wd, id, "pic.png", 1024).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        assert!(url.contains(
            &base64::engine::general_purpose::STANDARD.encode([0x89u8, b'P', b'N', b'G'])
        ));
        assert!(matches!(
            read_binary_data_url(&conn, &wd, id, "doc.txt", 1024),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            read_binary_data_url(&conn, &wd, id, "pic.png", 1),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn pdf_data_url_respects_kind_limit_and_magic() {
        let (wd, conn, id) = setup("fs-pdf");
        fs::write(root(&wd).join("doc.pdf"), b"%PDF-1.7\ncontent").unwrap();
        fs::write(root(&wd).join("fake.pdf"), b"renamed binary").unwrap();
        let url = read_binary_data_url(&conn, &wd, id, "doc.pdf", 1024).unwrap();
        assert!(url.starts_with("data:application/pdf;base64,"));
        assert!(url.contains(
            &base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.7\ncontent")
        ));
        assert!(matches!(
            read_binary_data_url(&conn, &wd, id, "fake.pdf", 1024),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            read_binary_data_url(&conn, &wd, id, "doc.pdf", 1),
            Err(AppError::Invalid(_))
        ));
        // a caller asking for more than the cap still gets the 25 MB clamp
        let pad = vec![b'x'; PDF_MAX_BYTES as usize + 1];
        fs::write(root(&wd).join("big.pdf"), [b"%PDF-", &pad[..]].concat()).unwrap();
        assert!(matches!(
            read_binary_data_url(&conn, &wd, id, "big.pdf", PDF_MAX_BYTES * 4),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }
}
