use crate::error::{AppError, Result};
use crate::fs_ops::{self, FsEntry};
use crate::paths;
use crate::state::AppState;
use rusqlite::{Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::AppHandle;

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

// Canonical-root variants of the resolvers below: the notes agent tools get
// the vault root once per run (ToolCtx.notes_root) instead of the working
// dir, and the wd-based public API just wraps them.
fn existing_at(root: &Path, rel: &str) -> Result<PathBuf> {
    fs_ops::contain_existing(root, rel, "Notes")
}

// Like existing_at(), but refuses the vault root itself and symlink entries.
fn existing_for_mutation_at(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.trim().is_empty() {
        return Err(AppError::Invalid(
            "cannot modify the Notes folder itself".into(),
        ));
    }
    let target = existing_at(root, rel)?;
    let joined = fs_ops::join_rel(root, rel)?;
    fs_ops::reject_symlink(&joined, rel)?;
    Ok(target)
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
    let joined = fs_ops::join_rel(root, rel)?;
    fs_ops::reject_symlink_chain(root, &joined, rel)?;
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

// Recursive delete of a note or folder; the vault root itself is refused.
pub fn delete(wd: &Path, rel: &str) -> Result<()> {
    delete_at(&vault_root(wd)?, rel)
}

pub(crate) fn delete_at(root: &Path, rel: &str) -> Result<()> {
    let path = existing_for_mutation_at(root, rel)?;
    if path.is_dir() {
        fs::remove_dir_all(&path)?;
    } else {
        fs::remove_file(&path)?;
    }
    Ok(())
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
    let chat_id = {
        let guard = state.db.lock().unwrap();
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
        chat_id
    };

    // Catalog first, prompt second: the generated tool block must cover the
    // whole notes catalog (schemas stay lazy).
    let catalog = crate::agent::notes_tools::notes_catalog();
    let skills = crate::skills::short_list(&wd);
    let system_prompt = crate::agent::prompts::notes_system_prompt(
        &skills,
        &catalog.summary_prompt_block(),
        &vault_summary(&wd),
    );

    crate::chat::spawn_run(
        app,
        &wd,
        registry,
        crate::chat::RunBridge {
            run: crate::state::RunKey::Notes,
            kind: "notes",
            chat_id,
            system_prompt,
            user_query: text.to_string(),
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

    // --- notes chat (SPEC notes chat slice) ---

    use crate::agent::permissions::DenyAllConfirmer;
    use crate::agent::r#loop as agent_loop;
    use crate::agent::r#loop::ChatBackend;
    use crate::chat;
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
            Some(agent_loop::AgentEvent::Done { answer, reason })
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
