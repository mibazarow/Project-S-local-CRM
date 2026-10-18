use crate::error::{AppError, Result};
use crate::sync_util::lock_recover;
use crate::fs_ops::{self, FsEntry};
use crate::paths;
use crate::path_policy;
use crate::state::AppState;
use rusqlite::{Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::AppHandle;

// Notes vault: a top-level Notes/ folder of the working dir holding markdown
// notes (folders allowed). All paths are vault-relative and resolved through
// the shared path_policy containment helpers, so escapes ("..", absolute
// paths, symlink hops) are rejected exactly like in project file operations.

pub const WELCOME_NOTE: &str = "Добро пожаловать.md";

const WELCOME_TEXT: &str = "# Заметки\n\n\
Здесь заметки Project-S — обычные Markdown-файлы и папки в рабочей папке.\n\n\
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
pub(crate) fn vault_root(wd: &Path) -> Result<PathBuf> {
    let dir = paths::notes_dir(wd);
    fs::create_dir_all(&dir)?;
    Ok(dir.canonicalize()?)
}

// Canonical-root variants of the resolvers below: the notes agent tools get
// the vault root once per run (ToolCtx.notes_root) instead of the working
// dir, and the wd-based public API just wraps them.
fn existing_at(root: &Path, rel: &str) -> Result<PathBuf> {
    path_policy::contain_existing(root, rel, "Notes")
}

// Like existing_at(), but refuses the vault root itself and symlink entries.
pub(crate) fn existing_for_mutation_at(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.trim().is_empty() {
        return Err(AppError::Invalid(
            "cannot modify the Notes folder itself".into(),
        ));
    }
    path_policy::contain_for_mutation(root, rel, "Notes")
}

// One folder level, sorted dirs-first case-insensitive (same order as the
// project file browser).
pub fn list(wd: &Path, rel: &str) -> Result<Vec<FsEntry>> {
    list_at(&vault_root(wd)?, rel)
}

pub(crate) fn list_at(root: &Path, rel: &str) -> Result<Vec<FsEntry>> {
    let dir = existing_at(root, rel)?;
    if !dir.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is not a folder")));
    }
    let mut entries = Vec::new();
    for item in fs::read_dir(&dir)? {
        let item = item?;
        if item.file_name() == ".DS_Store" {
            continue;
        }
        entries.push(fs_ops::entry(root, &item.path())?);
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
    read_at(&vault_root(wd)?, rel)
}

pub(crate) fn read_at(root: &Path, rel: &str) -> Result<String> {
    let path = existing_at(root, rel)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is a folder")));
    }
    fs_ops::read_text_at(&path, rel)
}

// Data URL preview for vault files the text reader cannot show (images and
// PDFs), same caps and magic checks as the project file panel; text files
// (including non-md) keep notes_read. Paths go through the vault policy, so
// escapes and symlink hops out of Notes/ are rejected.
pub fn read_binary_data_url(wd: &Path, rel: &str, max_bytes: u64) -> Result<String> {
    let root = vault_root(wd)?;
    let path = existing_at(&root, rel)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is a folder")));
    }
    fs_ops::binary_data_url_at(&root, &path, rel, max_bytes)
}

pub fn write(wd: &Path, rel: &str, content: &str) -> Result<FsEntry> {
    write_at(&vault_root(wd)?, rel, content)
}

pub(crate) fn write_at(root: &Path, rel: &str, content: &str) -> Result<FsEntry> {
    let path = existing_at(root, rel)?;
    if path.is_dir() {
        return Err(AppError::Invalid(format!("\"{rel}\" is a folder")));
    }
    fs::write(&path, content)?;
    fs_ops::entry(root, &path)
}

// Creates a new note at rel; parent folders are created on demand. A path
// component that already exists as a symlink is refused before anything is
// written, so auto-created parents cannot be redirected outside the vault.
pub fn create(wd: &Path, rel: &str, content: Option<&str>) -> Result<FsEntry> {
    create_at(&vault_root(wd)?, rel, content)
}

pub(crate) fn create_at(root: &Path, rel: &str, content: Option<&str>) -> Result<FsEntry> {
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
    let joined = path_policy::join_rel(root, rel)?;
    // Existing parent folders are matched leniently (dash/space/case/NFD
    // variants), so a variant note name lands in the real folder instead of
    // spawning a near-duplicate one; the note's own name stays as requested.
    let joined = path_policy::lenient_parent(&joined)?;
    path_policy::reject_symlink_chain(root, &joined, rel)?;
    if fs::symlink_metadata(&joined).is_ok() {
        return Err(AppError::Invalid(format!("\"{rel}\" already exists")));
    }
    if let Some(parent) = joined.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&joined, content.unwrap_or(""))?;
    fs_ops::entry(root, &joined)
}

pub fn rename(wd: &Path, rel: &str, new_name: &str) -> Result<FsEntry> {
    rename_at(&vault_root(wd)?, rel, new_name)
}

pub(crate) fn rename_at(root: &Path, rel: &str, new_name: &str) -> Result<FsEntry> {
    let name = fs_ops::check_name(new_name)?;
    let path = existing_for_mutation_at(root, rel)?;
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
    fs_ops::entry(root, &target)
}

// Recursive delete of a note or folder into Settings/.trash (reversible);
// the vault root itself is refused. Returns the trash row id.
pub fn delete(conn: &Connection, wd: &Path, rel: &str) -> Result<i64> {
    delete_at(conn, wd, &vault_root(wd)?, rel)
}

pub(crate) fn delete_at(conn: &Connection, wd: &Path, root: &Path, rel: &str) -> Result<i64> {
    let path = existing_for_mutation_at(root, rel)?;
    let kind = if path.is_dir() { "dir" } else { "file" };
    crate::reliability::trash_put(conn, wd, "notes", kind, &path, None)
}

// Creates a folder inside the vault (empty parent = vault root). The name is
// validated as a bare name, so it can never escape the validated parent.
pub fn mkdir(wd: &Path, parent_rel: &str, name: &str) -> Result<FsEntry> {
    mkdir_at(&vault_root(wd)?, parent_rel, name)
}

pub(crate) fn mkdir_at(root: &Path, parent_rel: &str, name: &str) -> Result<FsEntry> {
    let name = fs_ops::check_name(name)?;
    let parent = existing_at(root, parent_rel)?;
    if !parent.is_dir() {
        return Err(AppError::Invalid(format!(
            "\"{parent_rel}\" is not a folder"
        )));
    }
    let target = parent.join(&name);
    // symlink_metadata also catches dangling links, which exists() misses.
    if fs::symlink_metadata(&target).is_ok() {
        return Err(AppError::Invalid(format!("\"{name}\" already exists")));
    }
    fs::create_dir(&target)?;
    fs_ops::entry(root, &target)
}

// Moves a note or folder into an existing target folder (empty = vault root).
// Never overwrites: a name clash is an error. A folder cannot move into
// itself or into its own subfolder.
pub fn move_entry(wd: &Path, from_rel: &str, to_dir_rel: &str) -> Result<FsEntry> {
    move_at(&vault_root(wd)?, from_rel, to_dir_rel)
}

pub(crate) fn move_at(root: &Path, from_rel: &str, to_dir_rel: &str) -> Result<FsEntry> {
    let from = existing_for_mutation_at(root, from_rel)?;
    let dest_dir = existing_at(root, to_dir_rel)?;
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
    let target = dest_dir.join(&name);
    if target != from {
        if fs::symlink_metadata(&target).is_ok() {
            return Err(AppError::Invalid(format!("\"{name}\" already exists")));
        }
        fs::rename(&from, &target)?;
    }
    fs_ops::entry(root, &target)
}

// Copies a note or folder (recursively) into an existing target folder. On a
// name clash the copy gets the " copy" / " copy N" suffix, like duplicate.
pub fn copy(wd: &Path, from_rel: &str, to_dir_rel: &str) -> Result<FsEntry> {
    copy_at(&vault_root(wd)?, from_rel, to_dir_rel)
}

pub(crate) fn copy_at(root: &Path, from_rel: &str, to_dir_rel: &str) -> Result<FsEntry> {
    let from = existing_for_mutation_at(root, from_rel)?;
    let dest_dir = existing_at(root, to_dir_rel)?;
    if !dest_dir.is_dir() {
        return Err(AppError::Invalid(format!(
            "\"{to_dir_rel}\" is not a folder"
        )));
    }
    // Copying a folder into its own subtree would recurse forever.
    if from.is_dir() && dest_dir.starts_with(&from) {
        return Err(AppError::Invalid(
            "cannot copy a folder into itself or its own subfolder".into(),
        ));
    }
    let name = from
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = dest_dir.join(fs_ops::unique_name(&dest_dir, &name, from.is_dir()));
    if from.is_dir() {
        fs_ops::copy_tree(&from, &target)?;
    } else {
        fs::copy(&from, &target)?;
    }
    fs_ops::entry(root, &target)
}

// Copies Finder-dragged files and folders into the vault (SPEC 4.2 Finder
// DnD): copy only (the source stays put), folders recursively with their
// structure, collisions get the " copy" / " copy N" suffix. dest_rel "" is
// the vault root; escapes and symlinked target folders are rejected by the
// shared path policy, and the vault can never be imported into itself.
pub fn import(wd: &Path, dest_rel: &str, source_paths: Vec<String>) -> Result<Vec<FsEntry>> {
    let root = vault_root(wd)?;
    let dest = existing_at(&root, dest_rel)?;
    if !dest.is_dir() {
        return Err(AppError::Invalid(format!("\"{dest_rel}\" is not a folder")));
    }
    fs_ops::import_into(&root, &dest, source_paths)
}

// Quick capture (SPEC quick capture slice): append a timestamped bullet to
// the daily note Входящие/<YYYY-MM-DD>.md, creating the folder and the note
// (with its «# Входящие · <date>» header) on first use. Returns the
// vault-relative path of the daily note.
pub fn append_inbox(wd: &Path, text: &str) -> Result<String> {
    let text = text.trim().replace('\n', " ");
    if text.is_empty() {
        return Err(AppError::Invalid("capture text is empty".into()));
    }
    let root = vault_root(wd)?;
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d").to_string();
    let rel = format!("Входящие/{date}.md");
    let path = path_policy::join_rel(&root, &rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = format!("- {} — {text}", now.format("%H:%M"));
    let content = match fs::read_to_string(&path) {
        Ok(existing) => format!("{}\n{line}\n", existing.trim_end()),
        Err(_) => format!("# Входящие · {date}\n\n{line}\n"),
    };
    fs::write(&path, content)?;
    Ok(rel)
}

// Notes chat (SPEC notes chat slice): one continuous chat per working dir,
// a chats row with kind='notes' and no project/session. The partial unique
// index uniq_chats_one_notes_chat enforces the singleton, so ensure is
// get-or-create exactly like ensure_project_chat.
pub fn notes_chat_id(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM chats WHERE kind = 'notes'", [], |r| r.get(0))
        .optional()?)
}

pub fn ensure_notes_chat(conn: &Connection) -> Result<i64> {
    if let Some(id) = notes_chat_id(conn)? {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO chats (kind, created_at, updated_at)
         VALUES ('notes', datetime('now','localtime'), datetime('now','localtime'))",
        [],
    )?;
    Ok(conn.last_insert_rowid())
}

// UI-facing history for the notes chat (empty when no chat exists yet).
pub fn chat_history(conn: &Connection) -> Result<Vec<crate::chat::MessageDto>> {
    let Some(chat_id) = notes_chat_id(conn)? else {
        return Ok(Vec::new());
    };
    crate::chat::history_by_chat(conn, chat_id)
}

// Compact heuristic vault summary for the notes system prompt (no LLM):
// total markdown notes plus top-level folder names, so the agent starts
// with a rough map of the vault and must still read/search before quoting.
pub fn vault_summary(wd: &Path) -> String {
    let root = match vault_root(wd) {
        Ok(root) => root,
        Err(_) => return "Notes vault: not available.".to_string(),
    };
    let mut notes = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for item in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = item.path();
            if path.is_dir() {
                stack.push(path);
            } else if matches!(
                path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref(),
                Some("md") | Some("markdown")
            ) {
                notes += 1;
            }
        }
    }
    if notes == 0 {
        return "Notes vault: currently empty (no markdown notes yet).".to_string();
    }
    let mut folders: Vec<String> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|i| i.path().is_dir() && i.file_name() != ".DS_Store")
        .map(|i| i.file_name().to_string_lossy().into_owned())
        .collect();
    folders.sort();
    folders.truncate(20);
    if folders.is_empty() {
        format!("Notes vault summary: {notes} markdown notes; no top-level folders.")
    } else {
        format!(
            "Notes vault summary: {notes} markdown notes; top-level folders: {}.",
            folders.join(", ")
        )
    }
}

// Persists the user turn into the notes chat and starts the notes agent on a
// background thread (same shared bridge as project/harness runs); progress
// arrives as "agent-event" / "agent-confirmation" events carrying
// { chatId, notes: true }. Returns the notes chat id.
pub fn run_notes_chat(
    app: &AppHandle,
    state: &AppState,
    registry: &Arc<crate::state::AgentRegistry>,
    text: &str,
) -> Result<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("message text is empty".into()));
    }
    let wd = state.working_dir()?;
    ensure(&wd)?;
    // canonical root: the tools' containment compares canonical paths.
    let notes_root = vault_root(&wd)?;
    let (chat_id, memory_records) = {
        let guard = lock_recover(&state.db);
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let chat_id = ensure_notes_chat(conn)?;
        // The user turn is persisted first: build_context then sees the full
        // transcript (including it) when estimating and compacting.
        crate::chat::append_message(
            conn,
            chat_id,
            crate::llm::Role::User,
            Some(text),
            &[],
            None,
            None,
        )?;
        let memory_records = crate::memory::relevant_records(conn, None, text)?;
        (chat_id, memory_records)
    };

    // Catalog first, prompt second: the generated tool block must cover the
    // whole notes catalog (schemas stay lazy). The vault summary changes as
    // notes are written, so it is volatile: it rides the [Context] block of
    // the fresh user turn, keeping the system prompt byte-identical.
    let catalog = crate::agent::notes_tools::notes_catalog();
    let skills = crate::skills::short_list(&wd);
    let system_prompt =
        crate::agent::prompts::notes_system_prompt(&skills, &catalog.summary_prompt_block());
    let volatile_block =
        crate::chat::context::volatile_block(&[&vault_summary(&wd), &memory_records]);

    crate::chat::spawn_run(
        app,
        &wd,
        registry,
        crate::chat::RunBridge {
            run: crate::state::RunKey::Notes,
            kind: "notes",
            chat_id,
            system_prompt,
            volatile_block,
            catalog,
            notes_root: Some(notes_root),
        },
    );
    Ok(chat_id)
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

    #[test]
    fn lenient_reads_and_creates_resolve_special_name_variants() {
        let wd = setup("notes-lenient");
        let real = "Договор № 017-2026\u{a0}— копия.md";
        fs::write(notes(&wd).join(real), "note body").unwrap();
        assert_eq!(read(&wd, "Договор № 017-2026 - копия.md").unwrap(), "note body");
        // a variant of an existing folder name lands the new note in the real
        // folder instead of spawning a near-duplicate one
        fs::create_dir_all(notes(&wd).join("Точка\u{a0}— входа")).unwrap();
        let e = create(&wd, "Точка - входа/заметка.md", Some("x")).unwrap();
        assert!(notes(&wd).join("Точка\u{a0}— входа/заметка.md").is_file());
        assert!(e.rel_path.starts_with("Точка\u{a0}— входа/"), "{}", e.rel_path);
        fs::remove_dir_all(&wd).unwrap();
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
        // Rejections happen before any DB touch, an empty conn is enough.
        let conn = Connection::open_in_memory().unwrap();
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
                matches!(delete(&conn, &wd, bad), Err(AppError::Invalid(_))),
                "delete accepted: {bad:?}"
            );
        }
        assert!(matches!(delete(&conn, &wd, ""), Err(AppError::Invalid(_))));
        assert!(matches!(create(&wd, "", None), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn binary_data_url_reads_images_and_pdfs_rejects_other_types() {
        let wd = setup("notes-binary");
        fs::write(notes(&wd).join("pic.png"), [0x89, b'P', b'N', b'G', 1, 2]).unwrap();
        fs::write(notes(&wd).join("doc.pdf"), b"%PDF-1.7\ncontent").unwrap();
        fs::write(notes(&wd).join("archive.zip"), b"PK\x03\x04junk").unwrap();
        fs::write(notes(&wd).join("fake.pdf"), b"not a pdf").unwrap();
        fs::write(notes(&wd).join("notes.txt"), "plain text").unwrap();

        let url = read_binary_data_url(&wd, "pic.png", 1024).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        let pdf = read_binary_data_url(&wd, "doc.pdf", 1024).unwrap();
        assert!(pdf.starts_with("data:application/pdf;base64,"));
        for bad in ["archive.zip", "fake.pdf"] {
            assert!(
                matches!(read_binary_data_url(&wd, bad, 1024), Err(AppError::Invalid(_))),
                "accepted: {bad}"
            );
        }
        // Size caps are enforced like in the project panel.
        assert!(matches!(read_binary_data_url(&wd, "pic.png", 1), Err(AppError::Invalid(_))));
        assert!(matches!(read_binary_data_url(&wd, "doc.pdf", 1), Err(AppError::Invalid(_))));
        // Text files of any extension keep reading through notes_read, and
        // .md behavior is unchanged.
        assert_eq!(read(&wd, "notes.txt").unwrap(), "plain text");
        assert_eq!(read(&wd, WELCOME_NOTE).unwrap(), WELCOME_TEXT);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn binary_data_url_rejects_escapes_and_symlink_exits() {
        let wd = setup("notes-binary-escape");
        for bad in ["../x", "/etc/passwd", "a/../../x", "./x"] {
            assert!(
                matches!(read_binary_data_url(&wd, bad, 1024), Err(AppError::Invalid(_))),
                "accepted: {bad:?}"
            );
        }
        // A symlink inside the vault pointing outside it is rejected (the
        // canonicalized target leaves Notes/).
        use std::os::unix::fs::symlink;
        let outside = temp_test_dir("notes-binary-outside");
        fs::write(outside.join("secret.png"), [0x89, b'P', b'N', b'G']).unwrap();
        symlink(outside.join("secret.png"), notes(&wd).join("link.png")).unwrap();
        assert!(matches!(
            read_binary_data_url(&wd, "link.png", 1024),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&outside).unwrap();
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
        let conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            delete(&conn, &wd, "link.md"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            rename(&wd, "link.md", "x.md"),
            Err(AppError::Invalid(_))
        ));
        // move and copy refuse symlinks as sources, and a symlinked parent
        assert!(matches!(move_entry(&wd, "link.md", ""), Err(AppError::Invalid(_))));
        assert!(matches!(copy(&wd, "link.md", ""), Err(AppError::Invalid(_))));
        assert!(matches!(move_entry(&wd, "filelink", "dirlink"), Err(AppError::Invalid(_))));
        assert!(matches!(mkdir(&wd, "dirlink", "x"), Err(AppError::Invalid(_))));
        assert!(matches!(mkdir(&wd, "", "link.md"), Err(AppError::Invalid(_))));
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
        let conn = crate::db::open(&paths::db_path(&wd)).unwrap();
        create(&wd, "старое.md", Some("содержимое")).unwrap();
        let renamed = rename(&wd, "старое.md", "новое.md").unwrap();
        assert_eq!(renamed.rel_path, "новое.md");
        assert!(!notes(&wd).join("старое.md").exists());
        assert_eq!(read(&wd, "новое.md").unwrap(), "содержимое");
        assert!(rename(&wd, "новое.md", "новое.md").is_ok()); // same name is a no-op
        create(&wd, "новое.md", None).unwrap_err(); // name clash refused

        create(&wd, "папка/внутри.md", None).unwrap();
        delete(&conn, &wd, "папка").unwrap();
        assert!(!notes(&wd).join("папка").exists());
        delete(&conn, &wd, "новое.md").unwrap();
        assert!(!notes(&wd).join("новое.md").exists());
        assert!(matches!(delete(&conn, &wd, "нет-такого.md"), Err(AppError::Invalid(_))));
        // The deleted notes landed in the trash and can be put back.
        let trashed = crate::reliability::trash_list(&conn).unwrap();
        assert_eq!(trashed.len(), 2);
        crate::reliability::trash_restore(&conn, &wd, trashed[1].id).unwrap();
        assert!(notes(&wd).join("папка/внутри.md").is_file());
        assert_eq!(crate::reliability::trash_list(&conn).unwrap().len(), 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn mkdir_creates_folders_and_refuses_clashes_bad_names_and_escapes() {
        let wd = setup("notes-mkdir");
        let dir = mkdir(&wd, "", "Рабочие").unwrap();
        assert_eq!(dir.rel_path, "Рабочие");
        assert_eq!(dir.kind, "dir");
        assert!(notes(&wd).join("Рабочие").is_dir());
        assert_eq!(mkdir(&wd, "Рабочие", "2026").unwrap().rel_path, "Рабочие/2026");

        create(&wd, "файл.md", None).unwrap();
        for clash in ["Рабочие", "файл.md"] {
            assert!(
                matches!(mkdir(&wd, "", clash), Err(AppError::Invalid(_))),
                "clash accepted: {clash}"
            );
        }
        for bad in ["", " ", ".", "..", "a/b", "a\\b"] {
            assert!(
                matches!(mkdir(&wd, "", bad), Err(AppError::Invalid(_))),
                "name accepted: {bad:?}"
            );
        }
        for bad in ["../x", "/etc", "Рабочие/../..", "./Рабочие"] {
            assert!(
                matches!(mkdir(&wd, bad, "x"), Err(AppError::Invalid(_))),
                "parent accepted: {bad:?}"
            );
        }
        // parent must be an existing folder, not a file or a missing path
        assert!(matches!(mkdir(&wd, "файл.md", "x"), Err(AppError::Invalid(_))));
        assert!(matches!(mkdir(&wd, "нет-такого", "x"), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn move_carries_notes_and_folders_and_is_a_noop_in_place() {
        let wd = setup("notes-move");
        create(&wd, "нота.md", Some("текст")).unwrap();
        mkdir(&wd, "", "папка").unwrap();
        let moved = move_entry(&wd, "нота.md", "папка").unwrap();
        assert_eq!(moved.rel_path, "папка/нота.md");
        assert!(!notes(&wd).join("нота.md").exists());
        assert_eq!(read(&wd, "папка/нота.md").unwrap(), "текст");

        mkdir(&wd, "", "другая").unwrap();
        let dir_moved = move_entry(&wd, "папка", "другая").unwrap();
        assert_eq!(dir_moved.rel_path, "другая/папка");
        assert_eq!(dir_moved.kind, "dir");
        assert_eq!(read(&wd, "другая/папка/нота.md").unwrap(), "текст");

        // same parent and name is a no-op, not a collision
        let same = move_entry(&wd, "другая/папка", "другая").unwrap();
        assert_eq!(same.rel_path, "другая/папка");
        assert!(notes(&wd).join("другая/папка/нота.md").is_file());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn move_rejects_self_descendant_collisions_and_bad_destinations() {
        let wd = setup("notes-move-clash");
        mkdir(&wd, "", "a").unwrap();
        mkdir(&wd, "a", "b").unwrap();
        create(&wd, "c.md", Some("снаружи")).unwrap();
        create(&wd, "a/c.md", Some("внутри")).unwrap();

        for (from, to) in [("a", "a"), ("a", "a/b"), ("c.md", "a")] {
            assert!(
                matches!(move_entry(&wd, from, to), Err(AppError::Invalid(_))),
                "move accepted: {from} -> {to}"
            );
        }
        // nothing moved and nothing overwritten
        assert_eq!(read(&wd, "c.md").unwrap(), "снаружи");
        assert_eq!(read(&wd, "a/c.md").unwrap(), "внутри");
        assert!(notes(&wd).join("a/b").is_dir());

        // destination must be an existing folder; source must exist
        assert!(matches!(move_entry(&wd, "c.md", "c.md"), Err(AppError::Invalid(_))));
        assert!(matches!(move_entry(&wd, "c.md", "нет"), Err(AppError::Invalid(_))));
        assert!(matches!(move_entry(&wd, "нет.md", ""), Err(AppError::Invalid(_))));
        // the vault root itself is never a source
        assert!(matches!(move_entry(&wd, "", ""), Err(AppError::Invalid(_))));
        for bad in ["../x", "/etc/passwd", "a/../../x"] {
            assert!(
                matches!(move_entry(&wd, bad, ""), Err(AppError::Invalid(_))),
                "from accepted: {bad:?}"
            );
            assert!(
                matches!(move_entry(&wd, "c.md", bad), Err(AppError::Invalid(_))),
                "to accepted: {bad:?}"
            );
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn copy_keeps_the_source_and_suffixes_collisions() {
        let wd = setup("notes-copy");
        create(&wd, "нота.md", Some("раз")).unwrap();
        mkdir(&wd, "", "папка").unwrap();

        let copied = copy(&wd, "нота.md", "папка").unwrap();
        assert_eq!(copied.rel_path, "папка/нота.md");
        assert_eq!(read(&wd, "нота.md").unwrap(), "раз"); // source stays
        assert_eq!(read(&wd, "папка/нота.md").unwrap(), "раз");

        assert_eq!(copy(&wd, "нота.md", "").unwrap().rel_path, "нота copy.md");
        assert_eq!(copy(&wd, "нота.md", "").unwrap().rel_path, "нота copy 1.md");

        create(&wd, "папка/внутри.md", Some("два")).unwrap();
        let dir_copy = copy(&wd, "папка", "").unwrap();
        assert_eq!(dir_copy.rel_path, "папка copy");
        assert_eq!(dir_copy.kind, "dir");
        assert_eq!(read(&wd, "папка copy/внутри.md").unwrap(), "два");
        assert_eq!(read(&wd, "папка copy/нота.md").unwrap(), "раз");
        assert_eq!(copy(&wd, "папка", "").unwrap().rel_path, "папка copy 1");

        // a folder cannot be copied into its own subtree (infinite recursion)
        assert!(matches!(copy(&wd, "папка", "папка"), Err(AppError::Invalid(_))));
        assert!(matches!(copy(&wd, "папка", "папка/внутри.md"), Err(AppError::Invalid(_))));
        // a sibling folder is a fine destination, even for a copy of itself
        assert_eq!(copy(&wd, "папка", "папка copy").unwrap().rel_path, "папка copy/папка");
        assert!(matches!(copy(&wd, "нота.md", "нота.md"), Err(AppError::Invalid(_))));
        assert!(matches!(copy(&wd, "нота.md", "нет"), Err(AppError::Invalid(_))));
        assert!(matches!(copy(&wd, "нет.md", ""), Err(AppError::Invalid(_))));
        assert!(matches!(copy(&wd, "", ""), Err(AppError::Invalid(_))));
        for bad in ["../x", "/etc/passwd", "a/../../x"] {
            assert!(
                matches!(copy(&wd, bad, ""), Err(AppError::Invalid(_))),
                "from accepted: {bad:?}"
            );
            assert!(
                matches!(copy(&wd, "нота.md", bad), Err(AppError::Invalid(_))),
                "to accepted: {bad:?}"
            );
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn import_copies_files_and_folder_trees_into_the_vault() {
        let wd = setup("notes-import");
        let src = temp_test_dir("notes-import-src");
        fs::write(src.join("photo.png"), [1u8, 2, 3]).unwrap();
        fs::create_dir_all(src.join("docs/sub")).unwrap();
        fs::write(src.join("docs/sub/x.txt"), "y").unwrap();
        let photo = src.join("photo.png").display().to_string();
        let docs = src.join("docs").display().to_string();

        // into the vault root: files and whole folder trees land as copies
        let items = import(&wd, "", vec![photo.clone(), docs.clone()]).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].rel_path, "photo.png");
        assert!(items[0].is_image);
        assert_eq!(items[1].rel_path, "docs");
        assert!(notes(&wd).join("photo.png").is_file());
        assert!(notes(&wd).join("docs/sub/x.txt").is_file());
        // copy only: the dragged-from sources stay untouched
        assert!(src.join("photo.png").is_file());
        assert_eq!(fs::read_to_string(src.join("docs/sub/x.txt")).unwrap(), "y");

        // into an existing subfolder
        mkdir(&wd, "", "папка").unwrap();
        let into = import(&wd, "папка", vec![photo.clone()]).unwrap();
        assert_eq!(into[0].rel_path, "папка/photo.png");
        assert!(notes(&wd).join("папка/photo.png").is_file());

        // collision → unique suffix, the first copy stays intact
        let again = import(&wd, "", vec![photo.clone()]).unwrap();
        assert_eq!(again[0].rel_path, "photo copy.png");
        assert_eq!(import(&wd, "", vec![docs]).unwrap()[0].rel_path, "docs copy");

        // rejections: relative source paths, the vault itself, escaping and
        // non-folder destinations
        assert!(matches!(import(&wd, "", vec!["photo.png".into()]), Err(AppError::Invalid(_))));
        assert!(matches!(
            import(&wd, "", vec![notes(&wd).display().to_string()]),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            import(&wd, "папка", vec![notes(&wd).join("папка").display().to_string()]),
            Err(AppError::Invalid(_))
        ));
        for bad in ["../x", "/etc", "папка/../../x", "нет-такого"] {
            assert!(
                matches!(import(&wd, bad, vec![photo.clone()]), Err(AppError::Invalid(_))),
                "dest accepted: {bad:?}"
            );
        }
        create(&wd, "нота.md", None).unwrap();
        assert!(matches!(import(&wd, "нота.md", vec![photo]), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn import_rejects_symlinked_vault_dest() {
        use std::os::unix::fs::symlink;
        let wd = setup("notes-import-symlink");
        let src = temp_test_dir("notes-import-symlink-src");
        fs::write(src.join("f.txt"), "x").unwrap();
        let outside = temp_test_dir("notes-import-symlink-outside");
        fs::create_dir_all(outside.join("target")).unwrap();
        symlink(outside.join("target"), notes(&wd).join("dirlink")).unwrap();
        assert!(matches!(
            import(&wd, "dirlink", vec![src.join("f.txt").display().to_string()]),
            Err(AppError::Invalid(_))
        ));
        // nothing landed through the symlinked folder outside the vault
        assert!(!outside.join("target/f.txt").exists());
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&src).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn append_inbox_creates_daily_note_and_appends() {
        let wd = setup("notes-inbox");
        let rel = append_inbox(&wd, "первая мысль").unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(rel, format!("Входящие/{today}.md"));
        let content = read(&wd, &rel).unwrap();
        assert!(content.starts_with(&format!("# Входящие · {today}\n\n- ")), "{content}");
        assert!(content.ends_with(" — первая мысль\n"), "{content}");

        // second capture appends to the same daily note, header stays once
        append_inbox(&wd, "вторая мысль").unwrap();
        let content = read(&wd, &rel).unwrap();
        assert_eq!(content.lines().filter(|l| l.starts_with("- ")).count(), 2);
        assert_eq!(content.matches("# Входящие").count(), 1);

        // empty text is rejected, multiline collapses to one bullet
        assert!(matches!(append_inbox(&wd, "   "), Err(AppError::Invalid(_))));
        append_inbox(&wd, "две\nстроки").unwrap();
        let content = read(&wd, &rel).unwrap();
        assert!(content.contains("— две строки\n"), "{content}");
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
        let conn = crate::db::open(&paths::db_path(&wd)).unwrap();
        delete(&conn, &wd, "идеи.md").unwrap();
        let stats = index::index_notes(&wd).unwrap();
        assert_eq!(stats.files_removed, 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    // --- notes chat (SPEC notes chat slice) ---

    use crate::agent::r#loop as agent_loop;
    use crate::agent::r#loop::ChatBackend;
    use crate::chat;
    use crate::confirm::DenyAllConfirmer;
    use crate::db;
    use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, Role, ToolCall, ToolSpec};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn chat_conn() -> Connection {
        db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        db::schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn
    }

    #[test]
    fn ensure_notes_chat_is_a_singleton() {
        let conn = chat_conn();
        let first = ensure_notes_chat(&conn).unwrap();
        let second = ensure_notes_chat(&conn).unwrap();
        assert_eq!(first, second, "second call created another notes chat");
        let count: i64 = conn
            .query_row("SELECT count(*) FROM chats WHERE kind = 'notes'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // harness/project chats stay independent of the notes singleton
        conn.execute(
            "INSERT INTO chats (kind, created_at, updated_at) VALUES ('harness', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        // history is empty until the first turn
        let conn2 = chat_conn();
        assert!(chat_history(&conn2).unwrap().is_empty());
    }

    #[test]
    fn notes_history_persists_and_reloads() {
        let conn = chat_conn();
        let chat_id = ensure_notes_chat(&conn).unwrap();
        chat::append_message(&conn, chat_id, Role::User, Some("привет"), &[], None, None).unwrap();
        chat::append_message(&conn, chat_id, Role::Assistant, Some("ответ"), &[], None, None)
            .unwrap();
        let hist = chat_history(&conn).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].role, "user");
        assert_eq!(hist[0].content.as_deref(), Some("привет"));
        assert_eq!(hist[1].role, "assistant");
        assert_eq!(hist[1].content.as_deref(), Some("ответ"));
        assert_eq!(hist[0].position, 1);
        assert_eq!(hist[1].position, 2);
    }

    struct ScriptedBackend {
        rounds: Mutex<VecDeque<ChatOutcome>>,
        calls: AtomicUsize,
    }

    impl ScriptedBackend {
        fn new(rounds: Vec<ChatOutcome>) -> Self {
            ScriptedBackend {
                rounds: Mutex::new(rounds.into()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl ChatBackend for ScriptedBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .rounds
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| AppError::Invalid("scripted backend exhausted".into()))?;
            if let Some(content) = &outcome.content {
                on_event(ChatEvent::Delta(content.clone()));
            }
            Ok(outcome)
        }
    }

    fn tools_outcome(calls: Vec<ToolCall>) -> ChatOutcome {
        ChatOutcome {
            content: None,
            reasoning: None,
            tool_calls: calls,
            finish_reason: Some("tool_calls".into()),
            usage: None,
        }
    }

    // user → notes_read → finish_task, straight through the shared loop with
    // the real notes catalog and a real vault file (same persistence path as
    // run_notes_chat: on_message mirrors the transcript into the DB).
    #[test]
    fn scripted_notes_run_persists_transcript_and_emits_events() {
        let wd = setup("notes-chat-run");
        create(&wd, "заметка.md", Some("содержимое ноты")).unwrap();

        let conn = chat_conn();
        let chat_id = ensure_notes_chat(&conn).unwrap();
        chat::append_message(&conn, chat_id, Role::User, Some("прочитай заметку"), &[], None, None)
            .unwrap();

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![ToolCall::new(
                "c1".into(),
                "notes_read".into(),
                json!({ "relPath": "заметка.md" }).to_string(),
            )]),
            tools_outcome(vec![ToolCall::new(
                "f1".into(),
                "finish_task".into(),
                json!({ "action": "finish", "answer": "вот содержимое" }).to_string(),
            )]),
        ]);
        let mut catalog = crate::agent::notes_tools::notes_catalog();
        let mut agent_state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let config = agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user("прочитай заметку")],
            max_steps: 8,
            limits: agent_loop::RunLimits::default(),
            allow_all: false,
            project_id: None,
            session_id: None,
            chat_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
            },
            cancel: None,
            state: None,
            working_dir: Some(wd.clone()),
            notes_root: Some(paths::notes_dir(&wd).canonicalize().unwrap()),
        };
        let mut events = Vec::new();
        let mut on_message = |msg: &ChatMessage| {
            chat::append_message(
                &conn,
                chat_id,
                msg.role,
                msg.text_content(),
                &msg.tool_calls,
                msg.tool_call_id.as_deref(),
                msg.name.as_deref(),
            )
            .unwrap();
        };
        let result = agent_loop::run_with(
            &mut agent_state,
            &config,
            &DenyAllConfirmer,
            |e| events.push(e),
            &mut on_message,
        )
        .unwrap();
        chat::persist_final_answer(&conn, chat_id, &result).unwrap();
        assert_eq!(result.finish_reason, "finish");

        // Events in loop order: notes_read started/finished, finish_task
        // started/finished, Done with the answer.
        let kinds: Vec<String> = events
            .iter()
            .map(|e| match e {
                agent_loop::AgentEvent::ToolCallStarted { .. } => "started".into(),
                agent_loop::AgentEvent::ToolCallFinished { .. } => "finished".into(),
                agent_loop::AgentEvent::Done { .. } => "done".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kinds, ["started", "finished", "started", "finished", "done"]);
        assert!(matches!(
            events.last(),
            Some(agent_loop::AgentEvent::Done { answer, reason, .. })
                if answer == "вот содержимое" && reason == "finish"
        ));

        // DB mirrors the transcript: user, asst(notes_read), tool, asst(finish),
        // tool, asst(answer) — the tool envelope carries the real note text.
        let hist = chat::history_by_chat(&conn, chat_id).unwrap();
        let roles: Vec<&str> = hist.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            ["user", "assistant", "tool", "assistant", "tool", "assistant"]
        );
        assert_eq!(hist[1].tool_calls[0]["name"], "notes_read");
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert!(hist[2].content.as_deref().unwrap().contains("содержимое ноты"));
        assert_eq!(hist[5].content.as_deref(), Some("вот содержимое"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn vault_summary_counts_notes_and_lists_top_level_folders() {
        let wd = setup("notes-vault-summary");
        let fresh = vault_summary(&wd);
        assert!(fresh.contains("1 markdown notes"), "{fresh}"); // welcome note only
        create(&wd, "Рабочие/план.md", Some("пункт")).unwrap();
        create(&wd, "личное.md", Some("мысль")).unwrap();
        let summary = vault_summary(&wd);
        assert!(summary.contains("3 markdown notes"), "{summary}");
        assert!(summary.contains("Рабочие"), "{summary}");
        fs::remove_dir_all(&wd).unwrap();
    }
}
