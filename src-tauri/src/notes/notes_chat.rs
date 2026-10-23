// Notes chat (SPEC notes chat slice): one continuous chat per working dir,
// a chats row with kind='notes' and no project/session. The partial unique
// index uniq_chats_one_notes_chat enforces the singleton, so ensure is
// get-or-create exactly like ensure_project_chat. The vault file operations
// live in `fs`; the run startup below drives the shared chat bridge.
use crate::error::{AppError, Result};
use crate::state::AppState;
use crate::sync_util::lock_recover;
use rusqlite::{Connection, OptionalExtension};
use std::sync::Arc;
use tauri::AppHandle;

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

// Persists the user turn into the notes chat and starts the notes agent on a
// background thread (same shared bridge as project/harness runs); progress
// arrives as "agent-event" / "agent-confirmation" events carrying
// { chatId, notes: true }. Returns the notes chat id.
pub fn run_notes_chat(
    app: &AppHandle,
    state: &AppState,
    registry: &Arc<crate::state::AgentRegistry>,
    text: &str,
    attachments_in: &[crate::attachments::AttachmentRef],
) -> Result<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("message text is empty".into()));
    }
    let wd = state.working_dir()?;
    super::fs::ensure(&wd)?;
    // canonical root: the tools' containment compares canonical paths.
    let notes_root = super::fs::vault_root(&wd)?;
    let (chat_id, memory_records) = {
        let guard = lock_recover(&state.db);
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let chat_id = ensure_notes_chat(conn)?;
        // The user turn is persisted first: build_context then sees the full
        // transcript (including it) when estimating and compacting. Already
        // saved attachment copies (attach_files with notes: true) are linked to
        // it; load_history expands the turn for the model like project turns.
        let user_message = crate::chat::append_message(
            conn,
            chat_id,
            crate::llm::Role::User,
            Some(text),
            &[],
            None,
            None,
        )?;
        crate::attachments::save_message_attachments(conn, user_message.id, attachments_in)?;
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
        crate::chat::context::volatile_block(&[&super::fs::vault_summary(&wd), &memory_records]);

    crate::chat::spawn_run(
        app,
        &wd,
        registry,
        crate::chat::RunBridge {
            run: crate::state::RunKey::Notes,
            kind: "notes",
            chat_id,
            user_text: text.to_string(),
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
    use crate::agent::r#loop as agent_loop;
    use crate::agent::r#loop::ChatBackend;
    use crate::chat;
    use crate::confirm::DenyAllConfirmer;
    use crate::db;
    use crate::llm::{ChatEvent, ChatMessage, ChatOutcome, ChatParams, Role, ToolCall, ToolSpec};
    use crate::notes;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn setup(tag: &str) -> PathBuf {
        let wd = temp_test_dir(tag);
        notes::ensure(&wd).unwrap();
        wd
    }

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
        notes::create(&wd, "заметка.md", Some("содержимое ноты")).unwrap();

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
                requires_reasoning_passback: false,
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
}
