// Import/export (SPEC import/export slice): bring external folders, .zip
// archives and Obsidian vaults into a project or the Notes vault, and export
// a project or the whole vault as a .zip. Imports never overwrite silently:
// a name clash gets the " copy" / " copy N" suffix (fs_ops::unique_name), and
// every write stays inside the resolved target (contain_existing rejects
// "..", absolute paths and symlink hops). Archive extraction rejects absolute
// and parent-escaping entries and skips symlinks, mirroring skills::extract_zip.

use crate::error::{AppError, Result};
use crate::{fs_ops, notes, paths, projects};
use chrono::Local;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use zip::write::FileOptions;
use zip::ZipWriter;

// Folders an Obsidian vault keeps for itself; never part of the notes.
const OBSIDIAN_SKIP: &[&str] = &[".obsidian", ".trash", ".git"];

fn zip_err(e: zip::result::ZipError) -> AppError {
    AppError::Io(format!("zip: {e}"))
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub imported_files: u64,
    pub skipped: u64,
    pub dest: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub path: String,
    pub files: u64,
}

// Absolute, existing source path on disk.
fn canonical_source(source_path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(source_path);
    if !p.is_absolute() {
        return Err(AppError::Invalid(format!(
            "source path \"{source_path}\" must be absolute"
        )));
    }
    p.canonicalize()
        .map_err(|_| AppError::Invalid(format!("source \"{source_path}\" not found")))
}

// Refuses copying a folder into itself or into one of its own subfolders.
fn guard_not_self(src: &Path, root: &Path) -> Result<()> {
    if src == root || root.starts_with(src) {
        return Err(AppError::Invalid(
            "cannot import a folder into itself".into(),
        ));
    }
    Ok(())
}

// Canonical target root plus the existing destination folder inside it that
// imports land in. dest_rel "" means the target root itself.
fn resolve_dest(
    conn: &Connection,
    wd: &Path,
    target: &str,
    project_id: Option<i64>,
    dest_rel: &str,
) -> Result<(PathBuf, PathBuf)> {
    let root = match target {
        "project" => {
            let pid = project_id.ok_or_else(|| {
                AppError::Invalid("projectId is required for a project import".into())
            })?;
            projects::project_root(conn, wd, pid)?
        }
        "notes" => notes::vault_root(wd)?,
        other => {
            return Err(AppError::Invalid(format!(
                "unknown import target: {other} (project|notes)"
            )))
        }
    };
    let label = if target == "notes" { "Notes" } else { "project" };
    let dest = fs_ops::contain_existing(&root, dest_rel, label)?;
    if !dest.is_dir() {
        return Err(AppError::Invalid(format!("\"{dest_rel}\" is not a folder")));
    }
    Ok((root, dest))
}

fn count_files(root: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name() == ".DS_Store" {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(_) => total += 1,
                Err(_) => {}
            }
        }
    }
    total
}

// Copies `from` into `to` (already-suffixed target), skipping Obsidian's
// internal folders and symlinks; returns (files copied, folders skipped).
fn copy_tree_filtered(from: &Path, to: &Path) -> Result<(u64, u64)> {
    fs::create_dir_all(to)?;
    let mut files = 0;
    let mut skipped = 0;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for item in fs::read_dir(&src_dir)? {
            let item = item?;
            let name = item.file_name();
            let name_str = name.to_string_lossy();
            if name_str == ".DS_Store" {
                continue;
            }
            let file_type = item.file_type()?;
            if file_type.is_symlink() {
                skipped += 1;
                continue;
            }
            if file_type.is_dir() && OBSIDIAN_SKIP.iter().any(|s| name_str == *s) {
                skipped += 1;
                continue;
            }
            let dst = dst_dir.join(name);
            if file_type.is_dir() {
                fs::create_dir_all(&dst)?;
                stack.push((item.path(), dst));
            } else {
                fs::copy(item.path(), &dst)?;
                files += 1;
            }
        }
    }
    Ok((files, skipped))
}

// Extracts a .zip into `dest`, rejecting absolute and parent-escaping entries
// and skipping symlink entries (same containment as skills::extract_zip).
fn extract_zip_safe(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_err)?;
        let raw = entry.name().to_string();
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err(AppError::Invalid(format!(
                "zip entry has an absolute path: {raw:?}"
            )));
        }
        if entry.is_symlink() {
            continue;
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

// Copies a source folder into the target as a single named child (collision →
// suffix), the same shape as fs_ops::import for a directory source.
pub fn import_folder(
    conn: &Connection,
    wd: &Path,
    target: &str,
    project_id: Option<i64>,
    source_path: &str,
    dest_rel: &str,
) -> Result<ImportSummary> {
    let (root, dest) = resolve_dest(conn, wd, target, project_id, dest_rel)?;
    let src = canonical_source(source_path)?;
    if !src.is_dir() {
        return Err(AppError::Invalid(format!(
            "source \"{source_path}\" is not a folder"
        )));
    }
    guard_not_self(&src, &root)?;
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| AppError::Invalid(format!("source \"{source_path}\" has no name")))?;
    let landing = dest.join(fs_ops::unique_name(&dest, &name, true));
    fs_ops::copy_tree(&src, &landing)?;
    Ok(ImportSummary {
        imported_files: count_files(&landing),
        skipped: 0,
        dest: landing.display().to_string(),
    })
}

// Extracts a .zip into the target: each top-level archive entry lands directly
// in the destination folder, suffixed on a name clash. Extraction happens in a
// Harness/tmp staging folder first so an unsafe archive never touches the
// target, then the staging is removed.
pub fn import_archive(
    conn: &Connection,
    wd: &Path,
    target: &str,
    project_id: Option<i64>,
    source_path: &str,
    dest_rel: &str,
) -> Result<ImportSummary> {
    let (root, dest) = resolve_dest(conn, wd, target, project_id, dest_rel)?;
    let src = canonical_source(source_path)?;
    if !src.is_file() {
        return Err(AppError::Invalid(format!(
            "source \"{source_path}\" is not a file"
        )));
    }
    let is_zip = src
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase() == "zip")
        .unwrap_or(false);
    if !is_zip {
        return Err(AppError::Invalid(format!(
            "source \"{source_path}\" is not a .zip archive"
        )));
    }
    guard_not_self(&src, &root)?;

    let staging = paths::harness_tmp_dir(wd).join(format!("import-{}", unique_suffix()));
    fs::create_dir_all(&staging)?;
    let result = (|| -> Result<u64> {
        extract_zip_safe(&src, &staging)?;
        let mut files = 0;
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let from = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = from.is_dir();
            let landing = dest.join(fs_ops::unique_name(&dest, &name, is_dir));
            if is_dir {
                fs_ops::copy_tree(&from, &landing)?;
                files += count_files(&landing);
            } else {
                fs::copy(&from, &landing)?;
                files += 1;
            }
        }
        Ok(files)
    })();
    let _ = fs::remove_dir_all(&staging);
    Ok(ImportSummary {
        imported_files: result?,
        skipped: 0,
        dest: dest.display().to_string(),
    })
}

// Copies an Obsidian vault folder into Notes/ as a single named child,
// skipping .obsidian/.trash/.git and symlinks.
pub fn import_obsidian(wd: &Path, source_path: &str, dest_rel: &str) -> Result<ImportSummary> {
    let root = notes::vault_root(wd)?;
    let dest = fs_ops::contain_existing(&root, dest_rel, "Notes")?;
    if !dest.is_dir() {
        return Err(AppError::Invalid(format!("\"{dest_rel}\" is not a folder")));
    }
    let src = canonical_source(source_path)?;
    if !src.is_dir() {
        return Err(AppError::Invalid(format!(
            "source \"{source_path}\" is not a folder"
        )));
    }
    guard_not_self(&src, &root)?;
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| AppError::Invalid(format!("source \"{source_path}\" has no name")))?;
    let landing = dest.join(fs_ops::unique_name(&dest, &name, true));
    let (files, skipped) = copy_tree_filtered(&src, &landing)?;
    Ok(ImportSummary {
        imported_files: files,
        skipped,
        dest: landing.display().to_string(),
    })
}

// Writes every file under `root` into `dest` as a .zip, each entry prefixed
// with `prefix/` so the archive unpacks into one named folder. Returns the
// number of files written.
fn zip_tree(root: &Path, prefix: &str, dest: &Path) -> Result<u64> {
    let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
    let file = fs::File::create(dest)?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let mut count = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if entry.file_name() == ".DS_Store" {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|e| AppError::Io(format!("export rel path: {e}")))?
                .to_string_lossy()
                .replace('\\', "/");
            zip.start_file(format!("{prefix}/{rel}"), opts)
                .map_err(zip_err)?;
            let mut src = fs::File::open(&path)?;
            std::io::copy(&mut src, &mut zip)?;
            count += 1;
        }
    }
    zip.finish()
        .map_err(zip_err)?
        .into_inner()
        .map_err(|e| AppError::Io(format!("zip flush: {e}")))?
        .flush()?;
    Ok(count)
}

// Default export folder: Settings/backups/exports/.
fn exports_dir(wd: &Path) -> Result<PathBuf> {
    let dir = paths::backups_dir(wd).join("exports");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

// Zips a project folder. Archive layout: every entry sits under
// "<project-name>/", e.g. "example-ru/datas.md", "example-ru/tasks/1-x.md".
// Default destination Settings/backups/exports/<project>-<timestamp>.zip.
pub fn export_project(
    conn: &Connection,
    wd: &Path,
    project_id: i64,
    dest_path: Option<&Path>,
) -> Result<ExportSummary> {
    let root = projects::project_root(conn, wd, project_id)?;
    let name: String = conn
        .query_row("SELECT name FROM projects WHERE id = ?1", params![project_id], |r| r.get(0))
        .map_err(|_| AppError::Invalid(format!("project {project_id} not found")))?;
    let dest = match dest_path {
        Some(p) => p.to_path_buf(),
        None => exports_dir(wd)?.join(format!(
            "{name}-{}.zip",
            Local::now().format("%Y%m%d-%H%M%S")
        )),
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let files = zip_tree(&root, &name, &dest)?;
    Ok(ExportSummary {
        path: dest.display().to_string(),
        files,
    })
}

// Zips the whole Notes/ vault. Archive layout: every entry sits under
// "Notes/". Default destination Settings/backups/exports/notes-<timestamp>.zip.
pub fn export_notes(wd: &Path, dest_path: Option<&Path>) -> Result<ExportSummary> {
    let root = notes::vault_root(wd)?;
    let dest = match dest_path {
        Some(p) => p.to_path_buf(),
        None => exports_dir(wd)?.join(format!(
            "notes-{}.zip",
            Local::now().format("%Y%m%d-%H%M%S")
        )),
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let files = zip_tree(&root, "Notes", &dest)?;
    Ok(ExportSummary {
        path: dest.display().to_string(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;

    fn scaffold(tag: &str) -> (PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        notes::ensure(&wd).unwrap();
        (wd, conn, project.id)
    }

    fn make_zip(path: &Path, entries: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut w = ZipWriter::new(file);
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, content) in entries {
            w.start_file(name, opts).unwrap();
            std::io::Write::write_all(&mut w, content.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    fn zip_names(path: &Path) -> Vec<String> {
        let file = fs::File::open(path).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        archive.file_names().map(str::to_string).collect()
    }

    fn rel_paths(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn import_folder_into_project_lands_as_child_with_suffix() {
        let (wd, conn, pid) = scaffold("imp-folder");
        let src = temp_test_dir("imp-folder-src");
        fs::create_dir_all(src.join("docs/sub")).unwrap();
        fs::write(src.join("docs/a.md"), "a").unwrap();
        fs::write(src.join("docs/sub/b.txt"), "b").unwrap();

        let s = import_folder(&conn, &wd, "project", Some(pid), &src.join("docs").display().to_string(), "").unwrap();
        assert_eq!(s.imported_files, 2);
        let root = paths::projects_dir(&wd).join("example-ru");
        assert!(root.join("docs/a.md").is_file());
        assert!(root.join("docs/sub/b.txt").is_file());
        // dest is the canonical landing path (macOS /var → /private/var)
        assert_eq!(
            PathBuf::from(&s.dest),
            fs::canonicalize(root.join("docs")).unwrap()
        );

        // collision → suffix, never overwrite
        let again = import_folder(&conn, &wd, "project", Some(pid), &src.join("docs").display().to_string(), "").unwrap();
        assert_eq!(
            PathBuf::from(&again.dest),
            fs::canonicalize(root.join("docs copy")).unwrap()
        );
        assert!(root.join("docs copy/a.md").is_file());

        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn import_folder_into_notes_and_rejects_escapes() {
        let (wd, conn, _pid) = scaffold("imp-notes");
        let src = temp_test_dir("imp-notes-src");
        fs::create_dir_all(src.join("vault")).unwrap();
        fs::write(src.join("vault/n.md"), "n").unwrap();

        let s = import_folder(&conn, &wd, "notes", None, &src.join("vault").display().to_string(), "").unwrap();
        assert_eq!(s.imported_files, 1);
        assert!(paths::notes_dir(&wd).join("vault/n.md").is_file());

        // relative source rejected
        assert!(matches!(
            import_folder(&conn, &wd, "notes", None, "vault", ""),
            Err(AppError::Invalid(_))
        ));
        // dest_rel escaping the vault rejected
        assert!(matches!(
            import_folder(&conn, &wd, "notes", None, &src.join("vault").display().to_string(), "../x"),
            Err(AppError::Invalid(_))
        ));
        // importing the vault into itself rejected
        assert!(matches!(
            import_folder(&conn, &wd, "notes", None, &paths::notes_dir(&wd).display().to_string(), ""),
            Err(AppError::Invalid(_))
        ));
        // unknown target rejected
        assert!(matches!(
            import_folder(&conn, &wd, "bogus", None, &src.join("vault").display().to_string(), ""),
            Err(AppError::Invalid(_))
        ));
        // project target without projectId rejected
        assert!(matches!(
            import_folder(&conn, &wd, "project", None, &src.join("vault").display().to_string(), ""),
            Err(AppError::Invalid(_))
        ));

        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn import_archive_extracts_into_target_and_suffixes() {
        let (wd, conn, pid) = scaffold("imp-zip");
        let zip = wd.join("pack.zip");
        make_zip(&zip, &[("pack/a.md", "a"), ("pack/deep/b.txt", "b"), ("loose.md", "l")]);

        let s = import_archive(&conn, &wd, "project", Some(pid), &zip.display().to_string(), "").unwrap();
        assert_eq!(s.imported_files, 3);
        let root = paths::projects_dir(&wd).join("example-ru");
        assert!(root.join("pack/a.md").is_file());
        assert!(root.join("pack/deep/b.txt").is_file());
        assert!(root.join("loose.md").is_file());

        // second import: both top-level entries collide → suffixed, no overwrite
        fs::write(root.join("loose.md"), "keep").unwrap();
        let again = import_archive(&conn, &wd, "project", Some(pid), &zip.display().to_string(), "").unwrap();
        assert!(root.join("pack copy/a.md").is_file());
        assert!(root.join("loose copy.md").is_file());
        assert_eq!(fs::read_to_string(root.join("loose.md")).unwrap(), "keep");
        assert_eq!(again.imported_files, 3);

        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn import_archive_rejects_traversal_absolute_and_skips_symlinks() {
        let (wd, conn, pid) = scaffold("imp-zip-evil");
        let root = paths::projects_dir(&wd).join("example-ru");

        let evil = wd.join("evil.zip");
        make_zip(&evil, &[("ok.md", "ok"), ("../escape.md", "nope")]);
        let err = import_archive(&conn, &wd, "project", Some(pid), &evil.display().to_string(), "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsafe path"), "{err}");
        assert!(!wd.join("escape.md").exists());
        assert!(!root.join("escape.md").exists());

        let abs = wd.join("abs.zip");
        make_zip(&abs, &[("/abs/evil.md", "nope")]);
        let err = import_archive(&conn, &wd, "project", Some(pid), &abs.display().to_string(), "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute path"), "{err}");

        // a symlink entry is skipped, not extracted
        let link = wd.join("link.zip");
        {
            let file = fs::File::create(&link).unwrap();
            let mut w = ZipWriter::new(file);
            w.add_symlink("evil-link", "/etc/passwd", FileOptions::<()>::default())
                .unwrap();
            w.start_file("good.md", FileOptions::<()>::default()).unwrap();
            std::io::Write::write_all(&mut w, b"good").unwrap();
            w.finish().unwrap();
        }
        let s = import_archive(&conn, &wd, "project", Some(pid), &link.display().to_string(), "").unwrap();
        assert_eq!(s.imported_files, 1);
        assert!(root.join("good.md").is_file());
        assert!(!root.join("evil-link").exists());

        // non-zip source rejected
        let notzip = wd.join("plain.txt");
        fs::write(&notzip, "x").unwrap();
        assert!(matches!(
            import_archive(&conn, &wd, "project", Some(pid), &notzip.display().to_string(), ""),
            Err(AppError::Invalid(_))
        ));

        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn import_obsidian_skips_internal_folders_and_counts() {
        let (wd, _conn, _pid) = scaffold("imp-obsidian");
        let vault = temp_test_dir("imp-obsidian-vault");
        fs::create_dir_all(vault.join(".obsidian/plugins")).unwrap();
        fs::create_dir_all(vault.join(".trash")).unwrap();
        fs::create_dir_all(vault.join(".git/objects")).unwrap();
        fs::create_dir_all(vault.join("notes")).unwrap();
        fs::write(vault.join("index.md"), "i").unwrap();
        fs::write(vault.join("notes/page.md"), "p").unwrap();
        fs::write(vault.join("notes/img.png"), [1u8, 2, 3]).unwrap();
        fs::write(vault.join(".obsidian/config"), "cfg").unwrap();
        fs::write(vault.join(".trash/gone.md"), "gone").unwrap();
        fs::write(vault.join(".git/objects/x"), "x").unwrap();

        let s = import_obsidian(&wd, &vault.display().to_string(), "").unwrap();
        assert_eq!(s.imported_files, 3); // index.md + page.md + img.png
        assert_eq!(s.skipped, 3); // .obsidian + .trash + .git
        let landed = paths::notes_dir(&wd).join("imp-obsidian-vault");
        // the landing folder is named after the vault dir; assert by content instead
        let names = rel_paths(&paths::notes_dir(&wd).join(vault.file_name().unwrap()));
        assert_eq!(names, ["index.md", "notes/img.png", "notes/page.md"]);
        assert!(landed.parent().is_some());

        // collision → suffix
        let again = import_obsidian(&wd, &vault.display().to_string(), "").unwrap();
        let names2 = rel_paths(&paths::notes_dir(&wd).join(format!("{} copy", vault.file_name().unwrap().to_string_lossy())));
        assert_eq!(names2, ["index.md", "notes/img.png", "notes/page.md"]);
        assert_eq!(again.imported_files, 3);

        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&vault).unwrap();
    }

    #[test]
    fn export_project_zips_under_project_name_at_default_path() {
        let (wd, conn, pid) = scaffold("exp-proj");
        let root = paths::projects_dir(&wd).join("example-ru");
        fs::write(root.join("note.md"), "n").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/deep.txt"), "d").unwrap();
        fs::write(root.join(".DS_Store"), "junk").unwrap();

        let s = export_project(&conn, &wd, pid, None).unwrap();
        let path = PathBuf::from(&s.path);
        assert!(path.starts_with(paths::backups_dir(&wd).join("exports")));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("example-ru-") && name.ends_with(".zip"), "{name}");
        // datas.md + note.md + sub/deep.txt (no .DS_Store)
        assert_eq!(s.files, 3);
        let mut names = zip_names(&path);
        names.sort();
        assert_eq!(
            names,
            ["example-ru/datas.md", "example-ru/note.md", "example-ru/sub/deep.txt"]
        );

        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn export_notes_zips_under_notes_prefix_to_explicit_dest() {
        let (wd, _conn, _pid) = scaffold("exp-notes");
        fs::write(paths::notes_dir(&wd).join("a.md"), "a").unwrap();
        fs::create_dir_all(paths::notes_dir(&wd).join("sub")).unwrap();
        fs::write(paths::notes_dir(&wd).join("sub/b.md"), "b").unwrap();

        let out = temp_test_dir("exp-notes-out");
        let dest = out.join("chosen").join("vault.zip");
        let s = export_notes(&wd, Some(&dest)).unwrap();
        assert_eq!(PathBuf::from(&s.path), dest);
        assert_eq!(s.files, 3); // welcome + a.md + sub/b.md
        let mut names = zip_names(&dest);
        names.sort();
        assert!(names.iter().all(|n| n.starts_with("Notes/")));
        assert!(names.contains(&"Notes/sub/b.md".to_string()));

        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }
}
