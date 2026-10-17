use crate::error::{AppError, Result};
use crate::fs_ops::{self, FsEntry};
use crate::paths;
use std::fs;
use std::path::{Path, PathBuf};

// Notes vault: a top-level Notes/ folder of the working dir holding markdown
// notes (folders allowed). All paths are vault-relative and resolved through
// the shared fs_ops containment helpers, so escapes ("..", absolute paths,
// symlink hops) are rejected exactly like in project file operations.

pub const WELCOME_NOTE: &str = "Добро пожаловать.md";

const WELCOME_TEXT: &str = "# Заметки\n\n\
Здесь заметки Project S — обычные Markdown-файлы и папки в рабочей папке.\n\n\
- Всё из этой папки участвует в поиске: ключевые слова и семантика.\n\
- Файлы можно править в любом редакторе, структура папок — произвольная.\n";

// Creates Notes/ when missing and seeds the welcome note, but only when the
// folder was just created and is still empty. Idempotent.
pub fn ensure(wd: &Path) -> Result<PathBuf> {
    let dir = paths::notes_dir(wd);
    let fresh = !dir.exists();
    fs::create_dir_all(&dir)?;
    if fresh && fs::read_dir(&dir)?.next().is_none() {
        fs::write(dir.join(WELCOME_NOTE), WELCOME_TEXT)?;
    }
    Ok(dir)
}

// Canonical vault root, created on demand so reads work on a fresh dir.
fn vault_root(wd: &Path) -> Result<PathBuf> {
    let dir = paths::notes_dir(wd);
    fs::create_dir_all(&dir)?;
    Ok(dir.canonicalize()?)
}

fn existing(wd: &Path, rel: &str) -> Result<(PathBuf, PathBuf)> {
    let root = vault_root(wd)?;
    let target = fs_ops::contain_existing(&root, rel, "Notes")?;
    Ok((root, target))
}

// Like existing(), but refuses the vault root itself and symlink entries.
fn existing_for_mutation(wd: &Path, rel: &str) -> Result<(PathBuf, PathBuf)> {
    if rel.trim().is_empty() {
        return Err(AppError::Invalid(
            "cannot modify the Notes folder itself".into(),
        ));
    }
    let (root, target) = existing(wd, rel)?;
    let joined = fs_ops::join_rel(&root, rel)?;
    fs_ops::reject_symlink(&joined, rel)?;
    Ok((root, target))
}

// One folder level, sorted dirs-first case-insensitive (same order as the
// project file browser).
pub fn list(wd: &Path, rel: &str) -> Result<Vec<FsEntry>> {
    let (root, dir) = existing(wd, rel)?;
    if !dir.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is not a folder")));
    }
    let mut entries = Vec::new();
    for item in fs::read_dir(&dir)? {
        let item = item?;
        if item.file_name() == ".DS_Store" {
            continue;
        }
        entries.push(fs_ops::entry(&root, &item.path())?);
    }
    fs_ops::sort_entries(&mut entries);
    Ok(entries)
}

// Whole vault as one flat list (folders and files, vault-relative paths).
pub fn tree(wd: &Path) -> Result<Vec<FsEntry>> {
    let root = vault_root(wd)?;
    let mut entries = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for item in fs::read_dir(&dir)? {
            let item = item?;
            if item.file_name() == ".DS_Store" {
                continue;
            }
            let path = item.path();
            entries.push(fs_ops::entry(&root, &path)?);
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    fs_ops::sort_entries(&mut entries);
    Ok(entries)
}

pub fn read(wd: &Path, rel: &str) -> Result<String> {
    let (_, path) = existing(wd, rel)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is a folder")));
    }
    fs_ops::read_text_at(&path, rel)
}

pub fn write(wd: &Path, rel: &str, content: &str) -> Result<FsEntry> {
    let (root, path) = existing(wd, rel)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is a folder")));
    }
    fs::write(&path, content)?;
    fs_ops::entry(&root, &path)
}

// Creates a new note at rel; parent folders are created on demand. A path
// component that already exists as a symlink is refused before anything is
// written, so auto-created parents cannot be redirected outside the vault.
pub fn create(wd: &Path, rel: &str, content: Option<&str>) -> Result<FsEntry> {
    if rel.trim().is_empty() {
        return Err(AppError::Invalid("note path is empty".into()));
    }
    // join_rel strips "." and empty segments silently, so check the raw
    // segments: create must not turn "a/./b" or "a//b" into new folders.
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            return Err(AppError::Invalid(format!(
                "path \"{rel}\" is not Notes-relative"
            )));
        }
    }
    let root = vault_root(wd)?;
    let joined = fs_ops::join_rel(&root, rel)?;
    fs_ops::reject_symlink_chain(&root, &joined, rel)?;
    if fs::symlink_metadata(&joined).is_ok() {
        return Err(AppError::Invalid(format!("\"{rel}\" already exists")));
    }
    if let Some(parent) = joined.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&joined, content.unwrap_or(""))?;
    fs_ops::entry(&root, &joined)
}

pub fn rename(wd: &Path, rel: &str, new_name: &str) -> Result<FsEntry> {
    let name = fs_ops::check_name(new_name)?;
    let (root, path) = existing_for_mutation(wd, rel)?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Invalid("cannot rename the Notes folder".into()))?;
    let target = parent.join(&name);
    if target != path {
        if target.exists() {
            return Err(AppError::Invalid(format!("\"{name}\" already exists")));
        }
        fs::rename(&path, &target)?;
    }
    fs_ops::entry(&root, &target)
}

// Recursive delete of a note or folder; the vault root itself is refused.
pub fn delete(wd: &Path, rel: &str) -> Result<()> {
    let (_, path) = existing_for_mutation(wd, rel)?;
    if path.is_dir() {
        fs::remove_dir_all(&path)?;
    } else {
        fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use rusqlite::{params, Connection};

    fn setup(tag: &str) -> PathBuf {
        let wd = temp_test_dir(tag);
        ensure(&wd).unwrap();
        wd
    }

    fn notes(wd: &Path) -> PathBuf {
        paths::notes_dir(wd)
    }

    fn fts_rel_paths(wd: &Path, query: &str) -> Vec<String> {
        let conn = Connection::open(paths::db_path(wd)).unwrap();
        let mut stmt = conn
            .prepare("SELECT rel_path FROM files_fts WHERE files_fts MATCH ?1 AND project_id = ?2")
            .unwrap();
        stmt.query_map(params![query, index::NOTES_SCOPE_ID.to_string()], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn ensure_creates_vault_and_seeds_welcome_only_when_fresh_and_empty() {
        let wd = temp_test_dir("notes-ensure");
        assert!(!notes(&wd).exists());
        ensure(&wd).unwrap();
        assert!(notes(&wd).join(WELCOME_NOTE).is_file());

        // second run touches nothing
        fs::write(notes(&wd).join("свой.md"), "текст").unwrap();
        fs::remove_file(notes(&wd).join(WELCOME_NOTE)).unwrap();
        ensure(&wd).unwrap();
        assert!(!notes(&wd).join(WELCOME_NOTE).exists());
        assert_eq!(read(&wd, "свой.md").unwrap(), "текст");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn path_escapes_are_rejected() {
        let wd = setup("notes-escape");
        for bad in ["../x", "..", "/etc/passwd", "a/../../x", "./x", "a/./b"] {
            assert!(
                matches!(list(&wd, bad), Err(AppError::Invalid(_))),
                "list accepted: {bad:?}"
            );
            assert!(
                matches!(read(&wd, bad), Err(AppError::Invalid(_))),
                "read accepted: {bad:?}"
            );
            assert!(
                matches!(write(&wd, bad, "x"), Err(AppError::Invalid(_))),
                "write accepted: {bad:?}"
            );
            assert!(
                matches!(create(&wd, bad, None), Err(AppError::Invalid(_))),
                "create accepted: {bad:?}"
            );
            assert!(
                matches!(delete(&wd, bad), Err(AppError::Invalid(_))),
                "delete accepted: {bad:?}"
            );
        }
        assert!(matches!(delete(&wd, ""), Err(AppError::Invalid(_))));
        assert!(matches!(create(&wd, "", None), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escapes_are_rejected() {
        use std::os::unix::fs::symlink;
        let wd = setup("notes-symlink");
        symlink("/etc", notes(&wd).join("dirlink")).unwrap();
        symlink("/etc/hosts", notes(&wd).join("filelink")).unwrap();
        assert!(matches!(
            read(&wd, "filelink"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(list(&wd, "dirlink"), Err(AppError::Invalid(_))));
        assert!(matches!(
            write(&wd, "filelink", "x"),
            Err(AppError::Invalid(_))
        ));
        // mutating ops refuse even an in-vault symlink
        write(&wd, WELCOME_NOTE, "x").unwrap();
        symlink(notes(&wd).join(WELCOME_NOTE), notes(&wd).join("link.md")).unwrap();
        assert!(matches!(
            delete(&wd, "link.md"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            rename(&wd, "link.md", "x.md"),
            Err(AppError::Invalid(_))
        ));
        // create cannot be redirected through a symlinked parent
        assert!(matches!(
            create(&wd, "dirlink/evil.md", None),
            Err(AppError::Invalid(_))
        ));
        assert!(!Path::new("/tmp").join("evil.md").exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn crud_roundtrip() {
        let wd = setup("notes-crud");
        let created = create(&wd, "заметка.md", Some("раз")).unwrap();
        assert_eq!(created.rel_path, "заметка.md");
        assert!(created.is_markdown && created.is_text);
        assert_eq!(read(&wd, "заметка.md").unwrap(), "раз");
        let saved = write(&wd, "заметка.md", "два").unwrap();
        assert_eq!(saved.rel_path, "заметка.md");
        assert_eq!(read(&wd, "заметка.md").unwrap(), "два");
        assert!(matches!(
            create(&wd, "заметка.md", None),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_makes_parents_and_lists_nested() {
        let wd = setup("notes-nested");
        let item = create(&wd, "Рабочие/план.md", Some("пункт")).unwrap();
        assert_eq!(item.rel_path, "Рабочие/план.md");
        assert!(notes(&wd).join("Рабочие/план.md").is_file());
        let items = list(&wd, "").unwrap();
        let names: Vec<&str> = items.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Рабочие", WELCOME_NOTE]);
        assert_eq!(items[0].kind, "dir");
        let deep = list(&wd, "Рабочие").unwrap();
        assert_eq!(deep[0].rel_path, "Рабочие/план.md");
        assert!(matches!(list(&wd, "Рабочие/план.md"), Err(AppError::Invalid(_))));

        let all = tree(&wd).unwrap();
        let rels: Vec<&str> = all.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(rels, ["Рабочие", WELCOME_NOTE, "Рабочие/план.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn rename_and_recursive_delete_roundtrip() {
        let wd = setup("notes-roundtrip");
        create(&wd, "старое.md", Some("содержимое")).unwrap();
        let renamed = rename(&wd, "старое.md", "новое.md").unwrap();
        assert_eq!(renamed.rel_path, "новое.md");
        assert!(!notes(&wd).join("старое.md").exists());
        assert_eq!(read(&wd, "новое.md").unwrap(), "содержимое");
        assert!(rename(&wd, "новое.md", "новое.md").is_ok()); // same name is a no-op
        create(&wd, "новое.md", None).unwrap_err(); // name clash refused

        create(&wd, "папка/внутри.md", None).unwrap();
        delete(&wd, "папка").unwrap();
        assert!(!notes(&wd).join("папка").exists());
        delete(&wd, "новое.md").unwrap();
        assert!(!notes(&wd).join("новое.md").exists());
        assert!(matches!(delete(&wd, "нет-такого.md"), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn index_search_cyrillic_and_stale_rows_removed() {
        let wd = setup("notes-index");
        create(&wd, "счета/оплата.md", Some("Клиент example.ru оплатил счёт")).unwrap();

        let stats = index::index_notes(&wd).unwrap();
        assert_eq!(stats.files_indexed, 2); // welcome + оплата.md
        assert!(stats.errors.is_empty());
        assert_eq!(fts_rel_paths(&wd, "оплатил"), ["счета/оплата.md"]);

        // default temp wd has no embedding model: search degrades to FTS only
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        let hits = crate::search::search_notes(&state, "оплатил", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel_path, "счета/оплата.md");
        assert_eq!(hits[0].match_types, ["fts"]);
        assert!(hits[0].snippet.contains("оплатил"));

        fs::remove_file(notes(&wd).join("счета/оплата.md")).unwrap();
        let again = index::index_notes(&wd).unwrap();
        assert_eq!(again.files_removed, 1);
        assert!(fts_rel_paths(&wd, "оплатил").is_empty());
        assert!(crate::search::search_notes(&state, "оплатил", 10).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn notes_search_is_scoped_away_from_projects() {
        let wd = setup("notes-scope");
        create(&wd, "нота.md", Some("слово из заметки")).unwrap();
        index::index_notes(&wd).unwrap();

        let conn = crate::db::open(&paths::db_path(&wd)).unwrap();
        let project = crate::projects::create_project(&conn, &wd, "proj").unwrap();
        fs::write(
            paths::projects_dir(&wd).join("proj/файл.md"),
            "слово из проекта",
        )
        .unwrap();
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(crate::db::open(&paths::db_path(&wd)).unwrap());
        index::index_project(&state, project.id).unwrap();

        let hits = crate::search::search_notes(&state, "слово", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel_path, "нота.md");
        let project_hits = crate::search::search_project(&state, project.id, "слово", 10).unwrap();
        assert_eq!(project_hits.len(), 1);
        assert_eq!(project_hits[0].rel_path, "файл.md");
        fs::remove_dir_all(&wd).unwrap();
    }

    // Command-level smoke: the exact sequence the notes_* commands run.
    #[test]
    fn vault_smoke_flow() {
        let wd = setup("notes-smoke");
        create(&wd, "идея.md", Some("первая версия")).unwrap();
        assert_eq!(list(&wd, "").unwrap().len(), 2);
        assert_eq!(read(&wd, "идея.md").unwrap(), "первая версия");
        write(&wd, "идея.md", "вторая версия").unwrap();
        rename(&wd, "идея.md", "идеи.md").unwrap();
        assert_eq!(tree(&wd).unwrap().len(), 2);
        let stats = index::index_notes(&wd).unwrap();
        assert_eq!(stats.files_indexed, 2);
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        let hits = crate::search::search_notes(&state, "вторая", 10).unwrap();
        assert_eq!(hits[0].rel_path, "идеи.md");
        delete(&wd, "идеи.md").unwrap();
        let stats = index::index_notes(&wd).unwrap();
        assert_eq!(stats.files_removed, 1);
        fs::remove_dir_all(&wd).unwrap();
    }
}
