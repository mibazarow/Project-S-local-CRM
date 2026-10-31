// Chat persistence CRUD (SPEC 3.3, 4.6): the project chat get-or-create, the
// append-only message writes, the message DTO read path and the message-level
// transcript operations (pin / truncate / branch). The role and tool-call
// codecs live here because both the write path and `history` read them back.
use crate::attachments;
use crate::error::{AppError, Result};
use crate::llm::{ChatMessage, Role, ToolCall};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::search::sync_harness_message_index;
use super::types::{ChatBranchDto, MessageDto};

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

pub(crate) fn role_from_str(s: &str) -> Result<Role> {
    match s {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(AppError::Db(format!("unknown message role: {other}"))),
    }
}

// Tool calls are stored as a JSON array of {id, name, arguments}; arguments
// stay the raw model string so the wire shape reconstructs exactly.
fn tool_calls_to_json(calls: &[ToolCall]) -> Result<Option<String>> {
    if calls.is_empty() {
        return Ok(None);
    }
    let arr: Vec<Value> = calls
        .iter()
        .map(|tc| json!({ "id": tc.id, "name": tc.name, "arguments": tc.raw_arguments }))
        .collect();
    Ok(Some(serde_json::to_string(&arr)?))
}

pub(crate) fn tool_calls_from_json(text: &str) -> Result<Vec<ToolCall>> {
    let arr: Vec<Value> = serde_json::from_str(text)?;
    Ok(arr
        .iter()
        .map(|v| {
            ToolCall::new(
                v["id"].as_str().unwrap_or_default().to_string(),
                v["name"].as_str().unwrap_or_default().to_string(),
                v["arguments"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

fn parse_tool_calls_value(text: Option<String>) -> Vec<Value> {
    match text {
        Some(t) => serde_json::from_str(&t).unwrap_or_default(),
        None => Vec::new(),
    }
}

pub(crate) fn project_chat_id(conn: &Connection, project_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM chats WHERE project_id = ?1 AND kind = 'project'",
            params![project_id],
            |r| r.get(0),
        )
        .optional()?)
}

// Get-or-create: the partial unique index guarantees at most one row, so a
// second call returns the existing chat (SPEC 4.6 "один непрерывный чат").
// The builtin project's chat is seeded with one static assistant greeting
// (no LLM call) when it is created or still empty.
pub fn ensure_project_chat(conn: &Connection, project_id: i64) -> Result<i64> {
    if let Some(id) = project_chat_id(conn, project_id)? {
        seed_builtin_greeting(conn, project_id, id)?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO chats (project_id, kind, created_at, updated_at)
         VALUES (?1, 'project', datetime('now','localtime'), datetime('now','localtime'))",
        params![project_id],
    )?;
    let id = conn.last_insert_rowid();
    seed_builtin_greeting(conn, project_id, id)?;
    Ok(id)
}

fn seed_builtin_greeting(conn: &Connection, project_id: i64, chat_id: i64) -> Result<()> {
    let builtin = conn
        .query_row(
            "SELECT builtin FROM projects WHERE id = ?1",
            params![project_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|v| v != 0)
        .unwrap_or(false);
    if !builtin {
        return Ok(());
    }
    let messages: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get(0),
    )?;
    if messages == 0 {
        append_message(
            conn,
            chat_id,
            Role::Assistant,
            Some(crate::projects::BUILTIN_PROJECT_GREETING),
            &[],
            None,
            None,
        )?;
    }
    Ok(())
}

// Per-chat folder binding (Q.md «привязка чата к папке»): the folder chosen in
// the chat header, or None for the app-wide working dir. A stale path (folder
// removed or no longer a directory) is treated as unbound so a broken link
// cannot wedge the chat.
pub fn chat_working_dir(conn: &Connection, chat_id: i64) -> Result<Option<std::path::PathBuf>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT working_dir FROM chats WHERE id = ?1",
            rusqlite::params![chat_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(raw) = raw.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(raw);
    Ok(path.is_dir().then_some(path))
}

pub fn append_message(
    conn: &Connection,
    chat_id: i64,
    role: Role,
    content: Option<&str>,
    tool_calls: &[ToolCall],
    tool_call_id: Option<&str>,
    name: Option<&str>,
) -> Result<MessageDto> {
    append_full(conn, chat_id, role, content, tool_calls, tool_call_id, name, None)
}

// Persists a transcript message exactly as the agent loop appended it, its
// thinking trace included (assistant turns only).
pub fn append_chat_message(conn: &Connection, chat_id: i64, msg: &ChatMessage) -> Result<MessageDto> {
    append_full(
        conn,
        chat_id,
        msg.role,
        msg.text_content(),
        &msg.tool_calls,
        msg.tool_call_id.as_deref(),
        msg.name.as_deref(),
        msg.reasoning_content.as_deref(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_full(
    conn: &Connection,
    chat_id: i64,
    role: Role,
    content: Option<&str>,
    tool_calls: &[ToolCall],
    tool_call_id: Option<&str>,
    name: Option<&str>,
    reasoning: Option<&str>,
) -> Result<MessageDto> {
    let tool_calls_json = tool_calls_to_json(tool_calls)?;
    let position: i64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM messages WHERE chat_id = ?1",
        params![chat_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO messages (chat_id, role, content, reasoning, tool_calls, tool_call_id, name, created_at, position)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now','localtime'), ?8)",
        params![
            chat_id,
            role_str(role),
            content,
            reasoning,
            tool_calls_json,
            tool_call_id,
            name,
            position
        ],
    )?;
    conn.execute(
        "UPDATE chats SET updated_at = datetime('now','localtime') WHERE id = ?1",
        params![chat_id],
    )?;
    let message_id = conn.last_insert_rowid();
    sync_harness_message_index(conn, message_id, chat_id, role, content)?;
    message_dto(conn, message_id)
}

pub(crate) fn message_dto(conn: &Connection, id: i64) -> Result<MessageDto> {
    let mut dto = conn
        .query_row(
            "SELECT chat_id, role, content, reasoning, tool_calls, tool_call_id, name, created_at, position, pinned
             FROM messages WHERE id = ?1",
            params![id],
            |r| {
                Ok(MessageDto {
                    id,
                    chat_id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    reasoning: r.get(3)?,
                    tool_calls: parse_tool_calls_value(r.get(4)?),
                    tool_call_id: r.get(5)?,
                    name: r.get(6)?,
                    created_at: r.get(7)?,
                    position: r.get(8)?,
                    attachments: Vec::new(),
                    pinned: r.get::<_, i64>(9)? != 0,
                })
            },
        )
        .optional()?
        .ok_or_else(|| AppError::Db(format!("message {id} not found")))?;
    dto.attachments = attachments::load_message_attachments(conn, id)?;
    Ok(dto)
}

// ---------- Message-level chat UX (pin / truncate / branch)

// Pin or unpin one message of a chat; the UI shows pinned messages in a
// compact bar above the transcript.
pub fn set_pinned(conn: &Connection, chat_id: i64, message_id: i64, pinned: bool) -> Result<()> {
    let updated = conn.execute(
        "UPDATE messages SET pinned = ?1 WHERE id = ?2 AND chat_id = ?3",
        params![pinned as i64, message_id, chat_id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!(
            "message {message_id} not found in chat {chat_id}"
        )));
    }
    Ok(())
}

// Delete every message after `message_id` in the chat (regenerate flow):
// attachment links cascade via the FK, and the stale search mirrors (FTS +
// vectors) plus checkpoint summaries of the removed rows are dropped
// alongside so no deleted text stays searchable.
pub fn truncate_after(conn: &Connection, chat_id: i64, message_id: i64) -> Result<usize> {
    let boundary: Option<(i64, i64)> = conn
        .query_row(
            "SELECT id, position FROM messages WHERE id = ?1 AND chat_id = ?2",
            params![message_id, chat_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (boundary_id, boundary_position) = boundary
        .ok_or_else(|| AppError::Invalid(format!("message {message_id} not found in chat {chat_id}")))?;
    let removed_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE chat_id = ?1 AND id > ?2")?;
        let rows = stmt.query_map(params![chat_id, boundary_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in &removed_ids {
        // Best-effort mirrors: a missing row is fine — project and notes
        // chats were never in the harness search index.
        let _ = conn.execute("DELETE FROM messages_fts WHERE message_id = ?1", params![id]);
        let _ = conn.execute("DELETE FROM vec_messages WHERE rowid = ?1", params![id]);
        let _ = conn.execute("DELETE FROM message_vectors WHERE message_id = ?1", params![id]);
    }
    let deleted = conn.execute(
        "DELETE FROM messages WHERE chat_id = ?1 AND id > ?2",
        params![chat_id, boundary_id],
    )?;
    // Summaries folded past the cut describe messages that no longer exist.
    conn.execute(
        "DELETE FROM summaries WHERE chat_id = ?1 AND up_to_position > ?2",
        params![chat_id, boundary_position],
    )?;
    Ok(deleted)
}

// Fork a harness session at a message: a fresh session titled «Ветка · …»
// receives a copy of everything up to and including that message, so the
// conversation can take a new direction without losing the original. Project
// and notes chats have no session identity to fork — refused.
pub fn branch_chat(conn: &Connection, chat_id: i64, message_id: i64) -> Result<ChatBranchDto> {
    let kind: Option<String> = conn
        .query_row("SELECT kind FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))
        .optional()?;
    if kind.as_deref() != Some("harness") {
        return Err(AppError::Invalid(
            "branching is supported for harness chats only".into(),
        ));
    }
    let belongs: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE id = ?1 AND chat_id = ?2",
        params![message_id, chat_id],
        |r| r.get(0),
    )?;
    if belongs != 1 {
        return Err(AppError::Invalid(format!(
            "message {message_id} not found in chat {chat_id}"
        )));
    }
    let source_title: Option<String> = conn
        .query_row(
            "SELECT s.title FROM chats c JOIN sessions s ON s.id = c.session_id WHERE c.id = ?1",
            params![chat_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let base = source_title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Новая сессия");
    let session = crate::harness::create_session(conn, Some(&format!("Ветка · {base}")))?;
    let new_chat = crate::harness::session_chat_id(conn, session.id)?;
    conn.execute(
        "INSERT INTO messages (chat_id, role, content, reasoning, tool_calls, tool_call_id, name, created_at, position, pinned)
         SELECT ?1, role, content, reasoning, tool_calls, tool_call_id, name, created_at, position, pinned
         FROM messages WHERE chat_id = ?2 AND id <= ?3 ORDER BY position",
        params![new_chat, chat_id, message_id],
    )?;
    // Keep the harness search index consistent with the copied transcript
    // (same rules as sync_harness_message_index); vectors stay backfill-only.
    conn.execute(
        "INSERT INTO messages_fts (message_id, session_id, content)
         SELECT m.id, ?1, m.content FROM messages m
         WHERE m.chat_id = ?2 AND m.role IN ('user','assistant')
           AND m.content IS NOT NULL AND trim(m.content) <> ''",
        params![session.id, new_chat],
    )?;
    Ok(ChatBranchDto {
        chat_id: new_chat,
        session_id: session.id,
    })
}

// Shared test scaffolding for this module, its siblings and `run`: an
// in-memory DB with migrations plus a sample project, the tool-call literal
// and the wire-level tool-group check.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{ChatMessage, Connection, Role, ToolCall, Value};
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use std::fs;
    use std::path::PathBuf;

    pub(crate) fn setup(tag: &str) -> (PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        (wd, conn, dto.id)
    }

    pub(crate) fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall::new(id.into(), name.into(), args.to_string())
    }

    // Wire-level check: every assistant tool_calls turn is followed by a tool
    // message per tool_call_id before the next assistant turn (or the end).
    pub(crate) fn tool_groups_are_closed(messages: &[ChatMessage]) -> bool {
        let mut pending: Vec<String> = Vec::new();
        for m in messages {
            if m.role == Role::Tool {
                match m.tool_call_id.as_ref().and_then(|id| pending.iter().position(|p| p == id)) {
                    Some(pos) => { pending.remove(pos); }
                    None => return false,
                }
                continue;
            }
            if !pending.is_empty() {
                return false;
            }
            if m.role == Role::Assistant {
                pending = m.tool_calls.iter().map(|tc| tc.id.clone()).collect();
            }
        }
        pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{call, setup, tool_groups_are_closed};
    use super::*;
    use crate::attachments::AttachmentRef;
    use crate::chat::{chat_history, load_history};
    use serde_json::json;
    use std::fs;

    #[test]
    fn ensure_project_chat_returns_the_same_chat_for_a_project() {
        let (wd, conn, pid) = setup("chat-one-per-project");
        let first = ensure_project_chat(&conn, pid).unwrap();
        let second = ensure_project_chat(&conn, pid).unwrap();
        assert_eq!(first, second);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM chats WHERE kind='project'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn truncate_after_at_tool_call_turn_loads_a_valid_history() {
        let (wd, conn, pid) = setup("chat-sanitize-truncate");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();
        let calls = vec![
            call("c1", "read_file", json!({ "rel_path": "a.md" })),
            call("c2", "read_file", json!({ "rel_path": "b.md" })),
        ];
        let asst = append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        append_message(&conn, chat_id, Role::Tool, Some("r1"), &[], Some("c1"), None).unwrap();
        append_message(&conn, chat_id, Role::Tool, Some("r2"), &[], Some("c2"), None).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("done"), &[], None, None).unwrap();

        // Regenerate at the tool-call turn: the tool results go with the tail.
        assert_eq!(truncate_after(&conn, chat_id, asst.id).unwrap(), 3);
        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(hist[1].role, Role::Assistant);
        assert_eq!(hist[1].tool_calls.len(), 2);
        assert!(tool_groups_are_closed(&hist), "truncated history must be valid");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn branch_at_tool_call_turn_loads_a_valid_history() {
        let (wd, conn, _pid) = setup("chat-sanitize-branch");
        let session = crate::harness::create_session(&conn, Some("s")).unwrap();
        let chat_id = crate::harness::session_chat_id(&conn, session.id).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();
        let calls = vec![call("c1", "read_file", json!({ "rel_path": "a.md" }))];
        let asst = append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();

        // Branch right at the tool-call turn: the copy has no tool result.
        let branch = branch_chat(&conn, chat_id, asst.id).unwrap();
        let hist = load_history(&conn, branch.chat_id, false).unwrap();
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[2].role, Role::Tool);
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert!(tool_groups_are_closed(&hist), "branched history must be valid");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn pin_round_trip_marks_the_dto() {
        let (wd, conn, pid) = setup("chat-pin");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("важный вопрос"), &[], None, None).unwrap();
        assert!(!user.pinned);

        set_pinned(&conn, chat_id, user.id, true).unwrap();
        let dtos = chat_history(&conn, pid).unwrap();
        assert!(dtos[0].pinned);
        set_pinned(&conn, chat_id, user.id, false).unwrap();
        assert!(!chat_history(&conn, pid).unwrap()[0].pinned);

        // A message of another chat is refused, not silently ignored.
        assert!(matches!(set_pinned(&conn, 999, user.id, true), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn truncate_after_removes_trailing_messages_with_links_and_summaries() {
        let (wd, conn, pid) = setup("chat-truncate");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("вопрос"), &[], None, None).unwrap();
        let asst1 = append_message(&conn, chat_id, Role::Assistant, Some("первый"), &[], None, None).unwrap();
        attachments::save_message_attachments(
            &conn,
            asst1.id,
            &[AttachmentRef {
                name: "a.md".into(),
                saved_path: "Входящие/a.md".into(),
                kind: "document".into(),
                is_image: false,
            }],
        )
        .unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("второй"), &[], None, None).unwrap();
        conn.execute(
            "INSERT INTO summaries (chat_id, up_to_position, summary, created_at) VALUES (?1, ?2, 'складка', '2026-01-01')",
            params![chat_id, asst1.position],
        )
        .unwrap();

        assert_eq!(truncate_after(&conn, chat_id, user.id).unwrap(), 2);
        let hist = chat_history(&conn, pid).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].content.as_deref(), Some("вопрос"));

        // The deleted message's attachment link cascaded via the FK, and the
        // summary folded past the cut is gone.
        let links: i64 = conn
            .query_row("SELECT count(*) FROM message_attachments WHERE message_id = ?1", params![asst1.id], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 0);
        let summaries: i64 = conn
            .query_row("SELECT count(*) FROM summaries WHERE chat_id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        assert_eq!(summaries, 0);

        assert!(matches!(truncate_after(&conn, chat_id, 9999), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn truncate_after_drops_harness_search_mirrors() {
        let (wd, conn, _pid) = setup("chat-truncate-fts");
        let session = crate::harness::create_session(&conn, Some("сессия")).unwrap();
        let chat_id = crate::harness::session_chat_id(&conn, session.id).unwrap();
        let keep = append_message(&conn, chat_id, Role::User, Some("сохранить"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("удалить"), &[], None, None).unwrap();

        assert_eq!(truncate_after(&conn, chat_id, keep.id).unwrap(), 1);
        let fts: i64 = conn.query_row("SELECT count(*) FROM messages_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(fts, 1, "the removed turn must not stay searchable");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn branch_copies_transcript_into_a_new_harness_session() {
        let (wd, conn, pid) = setup("chat-branch");
        let session = crate::harness::create_session(&conn, Some("исходная")).unwrap();
        let chat_id = crate::harness::session_chat_id(&conn, session.id).unwrap();
        let m1 = append_message(&conn, chat_id, Role::User, Some("раз"), &[], None, None).unwrap();
        let m2 = append_message(&conn, chat_id, Role::Assistant, Some("два"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::User, Some("три"), &[], None, None).unwrap();

        let branch = branch_chat(&conn, chat_id, m2.id).unwrap();
        assert_ne!(branch.chat_id, chat_id);
        assert_ne!(branch.session_id, session.id);
        let title: String = conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![branch.session_id], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "Ветка · исходная");

        // Only everything up to and including the chosen message is copied.
        let hist = crate::harness::session_history(&conn, branch.session_id).unwrap();
        assert_eq!(hist.len(), 2);
        assert_ne!(hist[0].id, m1.id, "the branch holds copies, not shared rows");
        assert_eq!(hist[0].content.as_deref(), Some("раз"));
        assert_eq!(hist[1].content.as_deref(), Some("два"));
        let fts: i64 = conn
            .query_row("SELECT count(*) FROM messages_fts WHERE session_id = ?1", params![branch.session_id], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, 2, "the branch must be searchable right away");

        // The original transcript is untouched.
        assert_eq!(crate::harness::session_history(&conn, session.id).unwrap().len(), 3);

        // Project and notes chats have no session to fork — refused.
        let project_chat = ensure_project_chat(&conn, pid).unwrap();
        let pm = append_message(&conn, project_chat, Role::User, Some("x"), &[], None, None).unwrap();
        assert!(matches!(branch_chat(&conn, project_chat, pm.id), Err(AppError::Invalid(_))));
        let notes_chat = crate::notes::ensure_notes_chat(&conn).unwrap();
        let nm = append_message(&conn, notes_chat, Role::User, Some("y"), &[], None, None).unwrap();
        assert!(matches!(branch_chat(&conn, notes_chat, nm.id), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }
}
