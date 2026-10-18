// Harness sessions (SPEC 5.1, 5.2, 6): independent chats that share only the
// global memory. A session is its own identity row (title + TTL timestamps);
// its messages reuse the chats/messages tables through a kind='harness' chat
// linked by session_id, so message storage is not duplicated and deleting a
// session cascades to its messages. Schedules survive session deletion
// (session_id SET NULL, schema v12): a schedule whose session is gone creates
// a fresh session when it fires. The run path mirrors the project chat run
// and shares the agent loop and confirmation bridge.
use crate::agent::catalog::Catalog;
use crate::agent::prompts::harness_system_prompt;
use crate::attachments::{self, AttachmentRef};
use crate::chat::{self, RunBridge};
use crate::config::{self, ConfigKind, LlmConfig};
use crate::db;
use crate::error::{AppError, Result};
use crate::llm::{self, ChatParams, Role};
use crate::paths;
use crate::state::{AgentRegistry, AppState, RunKey};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    pub id: i64,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_active_at: String,
}

fn session_dto(conn: &Connection, id: i64) -> Result<SessionDto> {
    conn.query_row(
        "SELECT title, created_at, updated_at, last_active_at FROM sessions WHERE id = ?1",
        params![id],
        |r| {
            Ok(SessionDto {
                id,
                title: r.get(0)?,
                created_at: r.get(1)?,
                updated_at: r.get(2)?,
                last_active_at: r.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("session {id} not found")))
}

// The kind='harness' chat row created with the session: the message container
// the shared run path and history read against.
pub fn session_chat_id(conn: &Connection, session_id: i64) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM chats WHERE session_id = ?1 AND kind = 'harness'",
        params![session_id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("session {session_id} not found")))
}

pub fn create_session(conn: &Connection, title: Option<&str>) -> Result<SessionDto> {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    conn.execute(
        "INSERT INTO sessions (title, created_at, updated_at, last_active_at)
         VALUES (?1, datetime('now','localtime'), datetime('now','localtime'), datetime('now','localtime'))",
        params![title],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO chats (kind, session_id, created_at, updated_at)
         VALUES ('harness', ?1, datetime('now','localtime'), datetime('now','localtime'))",
        params![id],
    )?;
    session_dto(conn, id)
}

// Most recently active first (SPEC 5.1 session list).
pub fn list_sessions(conn: &Connection) -> Result<Vec<SessionDto>> {
    let ids: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT id FROM sessions ORDER BY last_active_at DESC, id DESC")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    ids.iter().map(|id| session_dto(conn, *id)).collect()
}

pub fn rename_session(conn: &Connection, id: i64, title: &str) -> Result<SessionDto> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::Invalid("session title is empty".into()));
    }
    let updated = conn.execute(
        "UPDATE sessions SET title = ?1, updated_at = datetime('now','localtime') WHERE id = ?2",
        params![title, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("session {id} not found")));
    }
    session_dto(conn, id)
}

// Cascades to the session's chat (→ messages, summaries); the session's
// schedules survive and detach (session_id SET NULL) to fire into a fresh
// session later.
pub fn delete_session(conn: &Connection, id: i64) -> Result<()> {
    let deleted = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
    if deleted != 1 {
        return Err(AppError::Invalid(format!("session {id} not found")));
    }
    Ok(())
}

pub fn touch_session(conn: &Connection, id: i64) -> Result<()> {
    let updated = conn.execute(
        "UPDATE sessions
         SET last_active_at = datetime('now','localtime'), updated_at = datetime('now','localtime')
         WHERE id = ?1",
        params![id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("session {id} not found")));
    }
    Ok(())
}

// Auto-title (SPEC 5.1): a session gets its title from the first user turn.
// Rule: the title is written only while sessions.title is NULL/empty. The
// frontend placeholder "Новая сессия" is never stored, and every manual
// rename (UI command and the rename_chat tool) requires a non-empty title,
// so a manual title is never overwritten; the guarded UPDATE below makes
// even a rename racing the auto-title pass win (0 rows updated). Sessions
// created with a meaningful default title (scheduler fires) are left alone.
const AUTOTITLE_MAX_TOKENS: u32 = 24;
const AUTOTITLE_LLM_MAX_CHARS: usize = 60;
const AUTOTITLE_FALLBACK_MAX_CHARS: usize = 40;

// First non-empty line cut to `max` chars: shared shape of the LLM answer
// cleanup and the no-LLM fallback.
fn first_line_cut(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let cut: String = line.chars().take(max).collect();
    cut.trim().to_string()
}

// No-LLM fallback: the first ~40 chars of the message, single line.
pub(crate) fn fallback_title(text: &str) -> String {
    first_line_cut(text, AUTOTITLE_FALLBACK_MAX_CHARS)
}

// LLM answers arrive quoted or with trailing punctuation: keep one plain
// line without quotes, cut to the cap; None when nothing usable is left.
pub(crate) fn sanitize_title(raw: &str) -> Option<String> {
    let line = first_line_cut(raw, AUTOTITLE_LLM_MAX_CHARS + 8);
    let line = line.trim_matches(|c: char| {
        matches!(c, '"' | '\'' | '«' | '»' | '`' | '.')
    });
    let cut: String = line.chars().take(AUTOTITLE_LLM_MAX_CHARS).collect();
    let title = cut.trim().to_string();
    (!title.is_empty()).then_some(title)
}

// Cheap one-shot LLM completion over the first user text; any failure
// (no config, no endpoint, empty answer) yields None so the caller falls
// back to truncation.
pub(crate) fn llm_title(wd: &Path, text: &str) -> Option<String> {
    let cfg: LlmConfig =
        serde_json::from_value(config::read_config(wd, ConfigKind::Llm).ok()?).ok()?;
    if cfg.base_url.trim().is_empty() {
        return None;
    }
    let client = llm::LlmClient::new(&cfg.base_url, &cfg.api_key);
    let params = ChatParams {
        model: cfg.model.clone(),
        temperature: None,
        max_tokens: Some(AUTOTITLE_MAX_TOKENS),
        reasoning_effort: None,
    };
    let prompt = format!("Give a short Russian title (2–5 words), no quotes\n\n{text}");
    let out = client
        .chat(&[llm::ChatMessage::user(&prompt)], None, &params)
        .ok()?;
    sanitize_title(out.content.as_deref()?)
}

// True while the session has no title yet (NULL/empty): the auto-title pass
// is only ever started for such sessions.
fn session_needs_title(conn: &Connection, session_id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT title IS NULL OR trim(title) = '' FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}

// Write the auto-title only while the session is still untitled; returns
// the applied title, or None when a (manual) title already exists.
pub(crate) fn apply_auto_title(conn: &Connection, session_id: i64, title: &str) -> Result<Option<String>> {
    let updated = conn.execute(
        "UPDATE sessions
         SET title = ?1, updated_at = datetime('now','localtime')
         WHERE id = ?2 AND (title IS NULL OR trim(title) = '')",
        params![title, session_id],
    )?;
    if updated == 1 {
        Ok(Some(title.to_string()))
    } else {
        Ok(None)
    }
}

// One auto-title pass on a caller-supplied connection: LLM title when the
// configured endpoint answers, truncation otherwise; applied only while the
// session is still untitled.
pub(crate) fn auto_title_now(conn: &Connection, wd: &Path, session_id: i64, text: &str) -> Result<Option<String>> {
    if !session_needs_title(conn, session_id)? {
        return Ok(None);
    }
    let title = llm_title(wd, text).unwrap_or_else(|| fallback_title(text));
    if title.is_empty() {
        return Ok(None);
    }
    apply_auto_title(conn, session_id, &title)
}

// Run the auto-title pass alongside the agent run: its own thread and DB
// connection (the run never waits on it), then "sessions-changed" so the
// sidebar picks the new title up as it arrives.
fn spawn_auto_title(app: &AppHandle, wd: &Path, session_id: i64, text: String) {
    let app = app.clone();
    let wd = wd.to_path_buf();
    std::thread::spawn(move || {
        let Ok(conn) = db::open(&paths::db_path(&wd)) else {
            return;
        };
        if let Ok(Some(_)) = auto_title_now(&conn, &wd, session_id, &text) {
            let _ = app.emit("sessions-changed", json!({}));
        }
    });
}

// SPEC 5.1: sessions idle for `days` days are auto-deleted; 0 disables purging.
// Returns the number of sessions removed (their chats/messages cascade; their
// schedules detach and keep firing into fresh sessions).
pub fn purge_inactive_sessions(conn: &Connection, days: u32) -> Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let deleted = conn.execute(
        "DELETE FROM sessions WHERE last_active_at < datetime('now','localtime', ?1)",
        params![format!("-{days} days")],
    )?;
    Ok(deleted)
}

pub fn session_history(conn: &Connection, session_id: i64) -> Result<Vec<chat::MessageDto>> {
    let chat_id = session_chat_id(conn, session_id)?;
    chat::history_by_chat(conn, chat_id)
}

// Harness tool set (SPEC 5.2): finish_task + skill + the shared global memory
// tools as the always-active core; the machine-wide fs/bash/CRM set is
// registered for lazy discovery (SPEC 6.4). CDP browser and scheduler tools
// land in later slices; the loop plugs the catalog into the run.
pub fn harness_catalog() -> Catalog {
    crate::agent::harness_tools::harness_catalog()
}

// Mirrors chat::start_project_run: persists the user turn (plus links to
// already-saved attachment copies, SPEC 6.1), touches the session, then runs
// the shared agent loop on a background thread with the harness prompt and
// catalog. Returns the session's chat id.
pub fn run_session(
    app: &AppHandle,
    state: &AppState,
    registry: &Arc<AgentRegistry>,
    session_id: i64,
    text: &str,
    kind: &'static str,
    attachments_in: &[AttachmentRef],
) -> Result<i64> {
    let guard = state.db.lock().unwrap();
    let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
    run_session_on(app, state, registry, conn, session_id, text, kind, attachments_in)
}

// Same as run_session but persists the user turn on a caller-supplied
// connection: the scheduler tick thread already holds the app DB mutex, so
// it must not be locked twice (std Mutex is not reentrant). The agent run
// itself always spawns its own thread with its own connection.
#[allow(clippy::too_many_arguments)]
pub fn run_session_on(
    app: &AppHandle,
    state: &AppState,
    registry: &Arc<AgentRegistry>,
    conn: &Connection,
    session_id: i64,
    text: &str,
    kind: &'static str,
    attachments_in: &[AttachmentRef],
) -> Result<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("message text is empty".into()));
    }
    let wd = state.working_dir()?;
    let (chat_id, needs_title, memory_records) = {
        let chat_id = session_chat_id(conn, session_id)?;
        let needs_title = session_needs_title(conn, session_id)?;
        touch_session(conn, session_id)?;
        let user_message =
            chat::append_message(conn, chat_id, Role::User, Some(text), &[], None, None)?;
        attachments::save_message_attachments(conn, user_message.id, attachments_in)?;
        // Harness runs carry no project: memory records are global, matched
        // against the fresh user turn, and ride the volatile [Context] block.
        let memory_records = crate::memory::relevant_records(conn, None, text)?;
        (chat_id, needs_title, memory_records)
    };
    if needs_title {
        // Auto-title from this first user turn, off the run path (own thread
        // + connection); the sidebar follows it through "sessions-changed".
        spawn_auto_title(app, &wd, session_id, text.to_string());
    }

    // Catalog first, prompt second: the generated tool block must cover the
    // whole harness catalog including the MCP bridges (schemas stay lazy).
    let mut catalog = harness_catalog();
    chat::register_mcp_tools(&mut catalog, state.mcp.lock().unwrap().tools());
    let skills = crate::skills::short_list(&wd);
    let system_prompt = harness_system_prompt(&skills, &catalog.summary_prompt_block());
    let volatile_block = crate::chat::context::volatile_block(&[&memory_records]);

    chat::spawn_run(
        app,
        &wd,
        registry,
        RunBridge {
            run: RunKey::Session(session_id),
            kind,
            chat_id,
            system_prompt,
            volatile_block,
            catalog,
            notes_root: None,
        },
    );
    Ok(chat_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::permissions::DenyAllConfirmer;
    use crate::agent::r#loop::{self as agent_loop, ChatBackend};
    use crate::chat::{append_message, load_history, persist_final_answer};
    use crate::db::schema;
    use crate::error::AppError;
    use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, ToolCall, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    struct ScriptedBackend {
        rounds: Mutex<VecDeque<ChatOutcome>>,
    }

    impl ScriptedBackend {
        fn new(rounds: Vec<ChatOutcome>) -> Self {
            ScriptedBackend {
                rounds: Mutex::new(rounds.into()),
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

    fn setup(tag: &str) -> (PathBuf, Connection) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        // Register the vec0 module before opening, like db::open does, so the
        // migration that creates vec_items works regardless of test order.
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        (wd, conn)
    }

    fn finish_call(answer: &str) -> ToolCall {
        ToolCall::new(
            "f1".into(),
            "finish_task".into(),
            json!({ "action": "finish", "answer": answer }).to_string(),
        )
    }

    #[test]
    fn sessions_crud_and_messages_persist_under_a_session() {
        let (wd, conn) = setup("harness-crud");
        let first = create_session(&conn, None).unwrap();
        assert_eq!(first.title, None);
        let second = create_session(&conn, Some("  Chat 2  ")).unwrap();
        assert_eq!(second.title.as_deref(), Some("Chat 2"));

        let chat_id = session_chat_id(&conn, first.id).unwrap();
        append_message(&conn, chat_id, Role::User, Some("hi"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("hello"), &[], None, None).unwrap();
        let hist = session_history(&conn, first.id).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].role, "user");
        assert_eq!(hist[1].content.as_deref(), Some("hello"));

        let renamed = rename_session(&conn, first.id, "Renamed").unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Renamed"));
        assert!(matches!(
            rename_session(&conn, 999, "x"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            session_history(&conn, 999),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn list_sessions_orders_by_last_active_desc() {
        let (wd, conn) = setup("harness-order");
        let a = create_session(&conn, Some("a")).unwrap();
        let b = create_session(&conn, Some("b")).unwrap();
        let c = create_session(&conn, Some("c")).unwrap();
        // Distinct timestamps so the second-granularity clock cannot tie.
        for (id, when) in [(a.id, "2026-01-01"), (b.id, "2026-01-03"), (c.id, "2026-01-02")] {
            conn.execute(
                "UPDATE sessions SET last_active_at = ?1 WHERE id = ?2",
                params![when, id],
            )
            .unwrap();
        }
        let order: Vec<i64> = list_sessions(&conn).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(order, vec![b.id, c.id, a.id]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_session_removes_messages_but_keeps_schedules_detached() {
        let (wd, conn) = setup("harness-delete");
        let s = create_session(&conn, Some("s")).unwrap();
        let chat_id = session_chat_id(&conn, s.id).unwrap();
        append_message(&conn, chat_id, Role::User, Some("hi"), &[], None, None).unwrap();
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, status, created_at) VALUES (?1, 'cron', '0 10 * * *', 'active', '2026-01-01')",
            params![s.id],
        )
        .unwrap();

        delete_session(&conn, s.id).unwrap();
        let messages: i64 = conn
            .query_row("SELECT count(*) FROM messages WHERE chat_id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        let chats: i64 = conn
            .query_row("SELECT count(*) FROM chats WHERE session_id = ?1", params![s.id], |r| r.get(0))
            .unwrap();
        // v12: the schedule survives session deletion with session_id NULL.
        let schedule_session: Option<i64> = conn
            .query_row("SELECT session_id FROM schedules WHERE kind = 'cron'", [], |r| r.get(0))
            .unwrap();
        assert_eq!((messages, chats), (0, 0));
        assert_eq!(schedule_session, None);
        assert!(matches!(delete_session(&conn, s.id), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn purge_inactive_sessions_deletes_only_old_and_zero_disables() {
        let (wd, conn) = setup("harness-purge");
        let old1 = create_session(&conn, Some("old1")).unwrap();
        let old2 = create_session(&conn, Some("old2")).unwrap();
        let fresh = create_session(&conn, Some("fresh")).unwrap();
        for id in [old1.id, old2.id] {
            conn.execute(
                "UPDATE sessions SET last_active_at = datetime('now','localtime','-40 days') WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }
        // 0 disables purging entirely.
        assert_eq!(purge_inactive_sessions(&conn, 0).unwrap(), 0);
        assert_eq!(purge_inactive_sessions(&conn, 30).unwrap(), 2);
        let remaining: Vec<i64> = list_sessions(&conn).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(remaining, vec![fresh.id]);
        // Cascade: the purged sessions' chats are gone too.
        let chats: i64 = conn
            .query_row("SELECT count(*) FROM chats WHERE session_id IN (?1, ?2)", params![old1.id, old2.id], |r| r.get(0))
            .unwrap();
        assert_eq!(chats, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn harness_catalog_has_finish_skill_memory_and_chat_tools_active() {
        let names: Vec<String> = harness_catalog()
            .to_specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        for expected in [
            "finish_task",
            "skill",
            "remember",
            "recall",
            "set_user_gender",
            "list_chats",
            "rename_chat",
            "new_chat",
            "delete_chat",
            "search_chats",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    // Session isolation (SPEC 5.1): a run's model context is built by
    // build_context strictly from the session's own chat (every read is
    // keyed by chat_id) plus the global memory block appended to the system
    // prompt. A message of one session must never appear in another
    // session's built context, while a global memory record is injected
    // into every session's run.
    #[test]
    fn session_context_is_isolated_and_global_memory_reaches_both() {
        let (wd, conn) = setup("harness-isolation");
        let a = create_session(&conn, Some("a")).unwrap();
        let b = create_session(&conn, Some("b")).unwrap();
        let chat_a = session_chat_id(&conn, a.id).unwrap();
        let chat_b = session_chat_id(&conn, b.id).unwrap();
        append_message(&conn, chat_a, Role::User, Some("MARKER-IN-A кофе"), &[], None, None).unwrap();
        append_message(&conn, chat_b, Role::User, Some("MARKER-IN-B кофе"), &[], None, None).unwrap();
        crate::memory::add(&conn, "GLOBAL-MEMORY-MARKER кофе", None, None, None).unwrap();

        let params = ChatParams {
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        };
        // Window far above the estimate: no compaction, no summarizer call.
        let backend = ScriptedBackend::new(vec![]);
        let dump = |chat_id: i64| {
            let out = crate::chat::context::build_context(
                &conn, chat_id, "sys", "", 100_000, 80, &backend, &params, false,
            )
            .unwrap();
            serde_json::to_string(&out.messages).unwrap()
        };
        let ctx_a = dump(chat_a);
        let ctx_b = dump(chat_b);
        assert!(ctx_a.contains("MARKER-IN-A") && !ctx_a.contains("MARKER-IN-B"), "{ctx_a}");
        assert!(ctx_b.contains("MARKER-IN-B") && !ctx_b.contains("MARKER-IN-A"), "{ctx_b}");

        // The global memory records are shared by design: the same record is
        // injected into both sessions' runs (harness runs carry no project).
        let block_a = crate::memory::relevant_records(&conn, None, "кофе").unwrap();
        let block_b = crate::memory::relevant_records(&conn, None, "кофе").unwrap();
        assert!(block_a.contains("GLOBAL-MEMORY-MARKER"), "{block_a}");
        assert!(block_b.contains("GLOBAL-MEMORY-MARKER"), "{block_b}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn touch_session_moves_it_to_the_top_of_the_list() {
        let (wd, conn) = setup("harness-touch-order");
        let a = create_session(&conn, Some("a")).unwrap();
        let b = create_session(&conn, Some("b")).unwrap();
        // Both pushed into the past; touching a makes it the freshest again.
        conn.execute(
            "UPDATE sessions SET last_active_at = '2026-01-01' WHERE id IN (?1, ?2)",
            params![a.id, b.id],
        )
        .unwrap();
        assert_eq!(list_sessions(&conn).unwrap()[0].id, b.id);
        touch_session(&conn, a.id).unwrap();
        let order: Vec<i64> = list_sessions(&conn).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(order, vec![a.id, b.id]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_title_falls_back_to_truncation_without_llm() {
        let (wd, conn) = setup("harness-autotitle-fallback");
        // No llm.json in the temp working dir: the LLM path fails and the
        // fallback (first ~40 chars, single line) applies.
        let s = create_session(&conn, None).unwrap();
        assert!(session_needs_title(&conn, s.id).unwrap());
        let text = "проверить конкурентов и подготовить отчёт по ценам\nвторая строка не должна попасть в заголовок";
        let applied = auto_title_now(&conn, &wd, s.id, text).unwrap();
        let title = applied.expect("fallback title applied");
        assert!(title.starts_with("проверить конкурентов"));
        assert!(!title.contains('\n'));
        assert!(title.chars().count() <= 40);
        assert_eq!(session_dto(&conn, s.id).unwrap().title.as_deref(), Some(title.as_str()));
        // A second pass never re-titles: the session already has a title.
        assert!(auto_title_now(&conn, &wd, s.id, text).unwrap().is_none());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_title_never_overwrites_a_manual_title() {
        let (wd, conn) = setup("harness-autotitle-manual");
        let s = create_session(&conn, Some("Вручную названа")).unwrap();
        assert!(!session_needs_title(&conn, s.id).unwrap());
        assert!(auto_title_now(&conn, &wd, s.id, "первое сообщение").unwrap().is_none());
        assert_eq!(
            session_dto(&conn, s.id).unwrap().title.as_deref(),
            Some("Вручную названа")
        );
        // The guarded UPDATE is race-safe: a title set after the pre-check
        // still wins (0 rows updated).
        let fresh = create_session(&conn, None).unwrap();
        rename_session(&conn, fresh.id, "Пользователь успел раньше").unwrap();
        assert!(apply_auto_title(&conn, fresh.id, "Авто").unwrap().is_none());
        assert_eq!(
            session_dto(&conn, fresh.id).unwrap().title.as_deref(),
            Some("Пользователь успел раньше")
        );
        // An empty-string title counts as untitled and gets auto-titled.
        conn.execute("UPDATE sessions SET title = '   ' WHERE id = ?1", params![fresh.id])
            .unwrap();
        assert!(session_needs_title(&conn, fresh.id).unwrap());
        assert!(apply_auto_title(&conn, fresh.id, "Авто").unwrap().is_some());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn sanitize_title_strips_quotes_punctuation_and_cuts_lines() {
        assert_eq!(
            sanitize_title("«Проверка сайта.»").as_deref(),
            Some("Проверка сайта")
        );
        assert_eq!(
            sanitize_title("  \"Отчёт по конкурентам\"  ").as_deref(),
            Some("Отчёт по конкурентам")
        );
        assert_eq!(
            sanitize_title("Заголовок\nлишний хвост").as_deref(),
            Some("Заголовок")
        );
        assert_eq!(sanitize_title("   ").as_deref(), None);
        assert_eq!(sanitize_title("«»").as_deref(), None);
        let long = "а".repeat(80);
        let cut = sanitize_title(&long).unwrap();
        assert_eq!(cut.chars().count(), 60);
        assert_eq!(fallback_title("одна строка"), "одна строка");
    }

    #[test]
    fn harness_run_persists_user_finish_and_final_answer() {
        let (wd, conn) = setup("harness-run");
        let s = create_session(&conn, Some("run")).unwrap();
        let chat_id = session_chat_id(&conn, s.id).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let backend = ScriptedBackend::new(vec![ChatOutcome {
            content: None,
            reasoning: None,
            tool_calls: vec![finish_call("final answer")],
            finish_reason: Some("tool_calls".into()),
            usage: None,
        }]);
        let mut catalog = harness_catalog();
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let config = agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user("go")],
            max_steps: 8,
            limits: agent_loop::RunLimits::default(),
            allow_all: false,
            project_id: None,
            session_id: None,
            chat_id: Some(chat_id),
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
            },
            cancel: None,
            state: None,
            working_dir: None,
            notes_root: None,
        };
        let mut on_message = |msg: &ChatMessage| {
            append_message(
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
        let result = agent_loop::run_with(&mut state, &config, &DenyAllConfirmer, |_| {}, &mut on_message)
            .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        let roles: Vec<Role> = hist.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(hist[1].tool_calls[0].name, "finish_task");
        assert_eq!(hist[3].text_content(), Some("final answer"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn cancel_is_keyed_by_run_not_a_bare_id() {
        let registry = AgentRegistry::new();
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry.set_cancel(RunKey::Session(1), &flag);
        // A project id of the same number must not touch the session's run.
        assert!(!registry.request_cancel(RunKey::Project(1)));
        assert!(!flag.load(Ordering::Relaxed));
        assert!(registry.request_cancel(RunKey::Session(1)));
        assert!(flag.load(Ordering::Relaxed));
        registry.clear_cancel(RunKey::Session(1));
        assert!(!registry.request_cancel(RunKey::Session(1)));
    }
}
