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
use crate::error::{AppError, Result};
use crate::llm::Role;
use crate::state::{AgentRegistry, AppState, RunKey};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::sync::Arc;
use tauri::AppHandle;

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
    let chat_id = {
        let chat_id = session_chat_id(conn, session_id)?;
        touch_session(conn, session_id)?;
        let user_message =
            chat::append_message(conn, chat_id, Role::User, Some(text), &[], None, None)?;
        attachments::save_message_attachments(conn, user_message.id, attachments_in)?;
        chat_id
    };

    let skills = crate::skills::short_list(&wd);
    let system_prompt = harness_system_prompt(&skills);
    let mcp_tools = state.mcp.lock().unwrap().tools();

    chat::spawn_run(
        app,
        &wd,
        registry,
        RunBridge {
            run: RunKey::Session(session_id),
            kind,
            chat_id,
            system_prompt,
            user_query: text.to_string(),
            catalog: harness_catalog(),
        },
        mcp_tools,
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
    fn harness_catalog_has_finish_skill_and_memory_tools_active() {
        let names: Vec<String> = harness_catalog()
            .to_specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        for expected in ["finish_task", "skill", "remember", "recall", "set_user_gender"] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn harness_run_persists_user_finish_and_final_answer() {
        let (wd, conn) = setup("harness-run");
        let s = create_session(&conn, Some("run")).unwrap();
        let chat_id = session_chat_id(&conn, s.id).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let backend = ScriptedBackend::new(vec![ChatOutcome {
            content: None,
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
            allow_all: false,
            project_id: None,
            session_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
            },
            cancel: None,
            state: None,
            working_dir: None,
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
