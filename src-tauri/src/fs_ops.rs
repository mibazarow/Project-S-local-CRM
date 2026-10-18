use crate::error::{AppError, Result};
use crate::projects;
use base64::Engine as _;
use rusqlite::Connection;
use serde::Serialize;
use std::fs;
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

pub fn duplicate(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<FsEntry> {
    let (root, path) = existing_for_mutation(conn, wd, project_id, rel_path)?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Invalid("cannot duplicate the project folder".into()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = parent.join(unique_name(parent, &name, path.is_dir()));
    if path.is_dir() {
        copy_tree(&path, &target)?;
    } else {
        fs::copy(&path, &target)?;
    }
    entry(&root, &target)
}

pub fn delete(conn: &Connection, wd: &Path, project_id: i64, rel_path: &str) -> Result<()> {
    let (_, path) = existing_for_mutation(conn, wd, project_id, rel_path)?;
    if path.is_dir() {
        fs::remove_dir_all(&path)?;
    } else {
        fs::remove_file(&path)?;
    }
    Ok(())
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

pub fn read_binary_data_url(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    rel_path: &str,
    max_bytes: u64,
) -> Result<String> {
    let (root, path) = existing(conn, wd, project_id, rel_path)?;
    let item = entry(&root, &path)?;
    if !item.is_image {
        return Err(AppError::Invalid(format!("\"{rel_path}\" is not an image")));
    }
    binary_data_url(&path, max_bytes, rel_path)
}

// Reads an image file into a base64 data URL, rejecting non-images and files
// over max_bytes. `label` names the file in errors. Shared by the
// project-scoped fs command above and the chat attachment preview, which
// resolves the path itself (attachments.rs).
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
pub(crate) fn contain_existing(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let joined = join_rel(root, rel)?;
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
    let (root, canon) = existing(conn, wd, project_id, rel)?;
    let joined = join_rel(&root, rel)?;
    reject_symlink(&joined, rel)?;
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
    let (stem, ext) = if is_dir {
        (name.to_owned(), String::new())
    } else {
        match name.rfind('.') {
            Some(i) if i > 0 => (name[..i].to_owned(), name[i..].to_owned()),
            _ => (name.to_owned(), String::new()),
        }
    };
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
    fn rename_duplicate_delete_roundtrip() {
        let (wd, conn, id) = setup("fs-roundtrip");
        create_file(&conn, &wd, id, "", "a.txt").unwrap();
        write_text(&conn, &wd, id, "a.txt", "one").unwrap();
        let renamed = rename(&conn, &wd, id, "a.txt", "b.txt").unwrap();
        assert_eq!(renamed.rel_path, "b.txt");
        assert!(!root(&wd).join("a.txt").exists());
        assert_eq!(read_text(&conn, &wd, id, "b.txt").unwrap(), "one");
        assert!(rename(&conn, &wd, id, "b.txt", "b.txt").is_ok()); // same name is a no-op

        let dup = duplicate(&conn, &wd, id, "b.txt").unwrap();
        assert_eq!(dup.rel_path, "b copy.txt");
        assert_eq!(read_text(&conn, &wd, id, "b copy.txt").unwrap(), "one");
        let dup2 = duplicate(&conn, &wd, id, "b.txt").unwrap();
        assert_eq!(dup2.rel_path, "b copy 1.txt");

        create_folder(&conn, &wd, id, "", "d").unwrap();
        create_file(&conn, &wd, id, "d", "inner.md").unwrap();
        let dir_dup = duplicate(&conn, &wd, id, "d").unwrap();
        assert_eq!(dir_dup.rel_path, "d copy");
        assert!(root(&wd).join("d copy/inner.md").is_file());

        delete(&conn, &wd, id, "d").unwrap();
        assert!(!root(&wd).join("d").exists());
        assert!(matches!(
            delete(&conn, &wd, id, ""),
            Err(AppError::Invalid(_))
        ));
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
}
