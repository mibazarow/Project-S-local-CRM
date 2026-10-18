// Chat persistence (SPEC 3.3, 4.6) plus the project-agent run bridge:
// send_project_message persists the user turn, spawns the agent loop on a
// background thread with its own DB connection (WAL-safe, same pattern as
// index/search), streams "agent-event" / "agent-confirmation" events to the
// UI, and mirrors every transcript message into the messages table through
// the loop's on_message hook — the DB copy can never diverge from what the
// model saw, and the transcript stays append-only.
pub mod context;

use crate::agent::catalog::Catalog;
use crate::agent::r#loop as agent_loop;
use crate::agent::permissions::{ConfirmDecision, ConfirmRequest, Confirmer};
use crate::agent::project_tools::project_catalog;
use crate::agent::prompts::project_system_prompt;
use crate::agent::tool::Tool;
use crate::attachments::{self, AttachmentRef};
use crate::config::{self, AppConfig, ConfigKind, LlmConfig};
use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::llm::{self, ChatMessage, ChatParams, ContentPart, MessageContent, Role, ToolCall};
use crate::memory;
use crate::paths;
use crate::projects;
use crate::state::{AgentRegistry, AppState, RunKey};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

// A project-agent turn may chain several tool rounds; the cap is a safety
// net, the real end is finish_task (SPEC 6.4).
pub const MAX_STEPS: usize = 24;
// A confirmation left unanswered (user away) denies rather than hangs.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: i64,
    pub chat_id: i64,
    pub role: String,
    pub content: Option<String>,
    // Thinking trace of an assistant turn (SPEC reasoning slice): replayed to
    // the model and shown collapsed in the UI.
    pub reasoning: Option<String>,
    pub tool_calls: Vec<Value>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub created_at: String,
    pub position: i64,
    // Saved attachment copies linked to this message (SPEC 6.1): user turns
    // carry files the user attached, assistant turns carry files the agent
    // sent with send_file. The raw content stays the plain text; the UI renders
    // the attachments as chips/thumbnails next to it.
    pub attachments: Vec<AttachmentRef>,
    // Pinned messages (SPEC chat UX slice) show in the compact bar above the
    // transcript; the flag lives on the message row itself.
    pub pinned: bool,
}

// Payload for the "agent-event" stream: one AgentEvent wrapped with its chat
// id plus the run routing key — projectId for project chats, sessionId for
// harness sessions (SPEC 6: one loop, one event stream, isolated contexts).
// Exactly one of the two ids is set; the other key is omitted from the JSON.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentEventPayload {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<i64>,
    // Set only for notes-chat runs: { chatId, notes: true, event }; the
    // project/harness payloads keep their exact previous shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<bool>,
    event: agent_loop::AgentEvent,
}

// Payload for the "agent-confirmation" UI event (SPEC 6: one confirmation
// queue for both agents), routed by the same run key as the event stream.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequestPayload {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<i64>,
    // Routed to the notes chat UI (see AgentEventPayload.notes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<bool>,
    pub tool: String,
    pub args: Value,
    pub danger_reason: Option<String>,
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn role_from_str(s: &str) -> Result<Role> {
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

fn tool_calls_from_json(text: &str) -> Result<Vec<ToolCall>> {
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

fn project_chat_id(conn: &Connection, project_id: i64) -> Result<Option<i64>> {
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
pub fn ensure_project_chat(conn: &Connection, project_id: i64) -> Result<i64> {
    if let Some(id) = project_chat_id(conn, project_id)? {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO chats (project_id, kind, created_at, updated_at)
         VALUES (?1, 'project', datetime('now','localtime'), datetime('now','localtime'))",
        params![project_id],
    )?;
    Ok(conn.last_insert_rowid())
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
fn append_full(
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

// Chat search index (schema v19): harness user/assistant turns are mirrored
// into messages_fts on append and embedded best-effort into vec_messages on a
// background thread. Project and notes chats stay out of scope (harness chat
// search only); tool turns and blank content are never indexed.
fn sync_harness_message_index(
    conn: &Connection,
    message_id: i64,
    chat_id: i64,
    role: Role,
    content: Option<&str>,
) -> Result<()> {
    if !matches!(role, Role::User | Role::Assistant) {
        return Ok(());
    }
    let text = match content.map(str::trim) {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(()),
    };
    let session_id: Option<i64> = conn
        .query_row(
            "SELECT session_id FROM chats WHERE id = ?1 AND kind = 'harness'",
            params![chat_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    let Some(session_id) = session_id else {
        return Ok(());
    };
    conn.execute(
        "INSERT INTO messages_fts (message_id, session_id, content) VALUES (?1, ?2, ?3)",
        params![message_id, session_id, content],
    )?;
    spawn_message_vector(conn, message_id, session_id, text);
    Ok(())
}

// The DB lives at <wd>/Settings/.db/project-s.db (paths::db_path), so the
// working dir is three levels up — where the embedder model files are. A DB
// outside that layout yields a wrong wd, the model check fails and the
// vector leg simply stays off.
fn working_dir_of_db(db_path: &Path) -> Option<PathBuf> {
    db_path.parent()?.parent()?.parent().map(Path::to_path_buf)
}

// Best-effort semantic leg: own thread, own connection (WAL-safe), every
// failure ignored — the FTS mirror above is the guaranteed index.
fn spawn_message_vector(conn: &Connection, message_id: i64, session_id: i64, text: &str) {
    let Some(db_path) = conn.path().map(PathBuf::from) else {
        return;
    };
    let text = text.to_owned();
    std::thread::spawn(move || {
        let Ok(thread_conn) = db::open(&db_path) else {
            return;
        };
        let Some(wd) = working_dir_of_db(&db_path) else {
            return;
        };
        let embedder = embed::select_embedder(&wd);
        if embedder.backend_id() == "noop" {
            return;
        }
        let _ = store_message_vector(&thread_conn, embedder.as_ref(), message_id, session_id, &text);
    });
}

// One vector per message under rowid = message_id; message_vectors tracks
// which messages already have one so the backfill skips them.
pub(crate) fn store_message_vector(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    message_id: i64,
    session_id: i64,
    text: &str,
) -> Result<()> {
    let dim = embedder.dim();
    let mut vector = embedder
        .embed_texts(&[embed::doc_prefix("", text)])?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Invalid("embedder returned no vector".into()))?;
    if vector.len() < dim {
        return Err(AppError::Invalid(format!(
            "embedder returned {} dims, need {dim}",
            vector.len()
        )));
    }
    embed::truncate_renorm(&mut vector, dim);
    let blob: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
    conn.execute("DELETE FROM vec_messages WHERE rowid = ?1", params![message_id])?;
    conn.execute(
        "INSERT INTO vec_messages (rowid, embedding) VALUES (?1, ?2)",
        params![message_id, blob],
    )?;
    conn.execute(
        "INSERT INTO message_vectors (message_id, session_id, updated_at)
         VALUES (?1, ?2, datetime('now','localtime'))
         ON CONFLICT(message_id) DO UPDATE SET
             session_id = excluded.session_id, updated_at = excluded.updated_at",
        params![message_id, session_id],
    )?;
    Ok(())
}

// Vector backfill for harness transcripts stored before their embeddings
// existed (FTS is already complete: v19 backfilled it and every append
// mirrors into it). Noop embedder yields 0, not an error.
pub fn index_chat_messages(state: &AppState) -> Result<usize> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let embedder = embed::select_embedder(&wd);
    index_chat_messages_with(&conn, embedder.as_ref())
}

pub(crate) fn index_chat_messages_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
) -> Result<usize> {
    if embedder.backend_id() == "noop" {
        return Ok(0);
    }
    let rows: Vec<(i64, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT m.id, c.session_id, m.content
             FROM messages m
             JOIN chats c ON c.id = m.chat_id AND c.kind = 'harness'
             LEFT JOIN message_vectors v ON v.message_id = m.id
             WHERE c.session_id IS NOT NULL
               AND m.role IN ('user','assistant')
               AND m.content IS NOT NULL AND trim(m.content) <> ''
               AND v.message_id IS NULL
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut indexed = 0usize;
    for (message_id, session_id, content) in rows {
        // One failing embed must not abort the whole backfill.
        if store_message_vector(conn, embedder, message_id, session_id, &content).is_ok() {
            indexed += 1;
        }
    }
    Ok(indexed)
}

fn message_dto(conn: &Connection, id: i64) -> Result<MessageDto> {
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

// Rebuilds the model-facing transcript (system prompt excluded: the loop
// regenerates it per run). User turns that carry attachments are expanded
// through attachments::build_user_turn: documents contribute their extracted
// text, and images become image_url content parts when `vision_supported`
// (otherwise the turn carries an honest note and the image is not sent). The
// stored rows keep the plain user text; only this model-facing view is expanded.
pub fn load_history(
    conn: &Connection,
    chat_id: i64,
    vision_supported: bool,
) -> Result<Vec<ChatMessage>> {
    let base = chat_project_root(conn, chat_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, role, content, reasoning, tool_calls, tool_call_id, name
         FROM messages WHERE chat_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map(params![chat_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, role, content, reasoning, tool_calls, tool_call_id, name) = row?;
        let content = if role == "user" {
            expand_user_turn(conn, id, content, base.as_deref(), vision_supported)?
        } else {
            content.map(MessageContent::Text)
        };
        out.push(ChatMessage {
            role: role_from_str(&role)?,
            content,
            tool_calls: match tool_calls {
                Some(t) => tool_calls_from_json(&t)?,
                None => Vec::new(),
            },
            tool_call_id,
            name,
            // Thinking traces of stored assistant turns travel back to the
            // model: thinking-mode providers reject a follow-up whose previous
            // assistant turn dropped `reasoning_content`.
            reasoning_content: reasoning,
        });
    }
    Ok(out)
}

// Canonical project root for the chat's project; None for harness chats,
// whose attachment savedPaths are absolute anyway.
fn chat_project_root(conn: &Connection, chat_id: i64) -> Result<Option<PathBuf>> {
    Ok(conn
        .query_row(
            "SELECT p.path FROM chats c JOIN projects p ON p.id = c.project_id WHERE c.id = ?1",
            params![chat_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from))
}

// Rebuilds a user turn with its attachment blocks; messages without stored
// attachments keep their content untouched. When vision is supported and an
// image is present, the turn becomes a multimodal parts array (text part plus
// one image_url part per image); otherwise it stays a plain string with the
// document text injected and an honest note for any image.
fn expand_user_turn(
    conn: &Connection,
    message_id: i64,
    content: Option<String>,
    base: Option<&Path>,
    vision_supported: bool,
) -> Result<Option<MessageContent>> {
    let refs = attachments::load_message_attachments(conn, message_id)?;
    if refs.is_empty() {
        return Ok(content.map(MessageContent::Text));
    }
    let dtos: Vec<attachments::AttachmentDto> = refs.iter().map(attachments::ref_to_dto).collect();
    let text = content.unwrap_or_default();
    let turn = attachments::build_user_content(
        &text,
        &dtos,
        base.unwrap_or(Path::new("")),
        vision_supported,
    );
    if turn.images.is_empty() {
        return Ok(Some(MessageContent::Text(turn.text)));
    }
    let mut parts = Vec::with_capacity(turn.images.len() + 1);
    if !turn.text.is_empty() {
        parts.push(ContentPart::text(turn.text));
    }
    for url in turn.images {
        parts.push(ContentPart::image_url(url));
    }
    Ok(Some(MessageContent::Parts(parts)))
}

// UI-facing history for a chat by its id (shared by project chats and harness
// sessions).
pub fn history_by_chat(conn: &Connection, chat_id: i64) -> Result<Vec<MessageDto>> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM messages WHERE chat_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map(params![chat_id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    ids.iter().map(|id| message_dto(conn, *id)).collect()
}

// UI-facing history for the project's chat (empty when no chat exists yet).
pub fn chat_history(conn: &Connection, project_id: i64) -> Result<Vec<MessageDto>> {
    let Some(chat_id) = project_chat_id(conn, project_id)? else {
        return Ok(Vec::new());
    };
    history_by_chat(conn, chat_id)
}

// ---------- Message-level chat UX (pin / truncate / branch / export / search)

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatBranchDto {
    pub chat_id: i64,
    pub session_id: i64,
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

// Readable Markdown transcript of a chat for export: role headers with
// timestamps, reasoning in a collapsible block, a compact one-line note per
// tool round; tool results and system turns are skipped as noise.
pub fn export_markdown(conn: &Connection, chat_id: i64) -> Result<String> {
    let exists: i64 = conn
        .query_row("SELECT count(*) FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))?;
    if exists != 1 {
        return Err(AppError::Invalid(format!("chat {chat_id} not found")));
    }
    let mut out = format!("# Чат #{chat_id}\n");
    for m in history_by_chat(conn, chat_id)? {
        if m.role != "user" && m.role != "assistant" {
            continue;
        }
        let who = if m.role == "user" { "Вы" } else { "Агент" };
        out.push_str(&format!("\n## {who} · {}\n\n", m.created_at));
        if let Some(reasoning) = m.reasoning.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            out.push_str(&format!(
                "<details><summary>Размышления</summary>\n\n{reasoning}\n\n</details>\n\n"
            ));
        }
        if let Some(content) = m.content.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            out.push_str(content);
            out.push('\n');
        }
        let tools: Vec<String> = m
            .tool_calls
            .iter()
            .filter_map(|tc| tc["name"].as_str().map(str::to_string))
            .collect();
        if !tools.is_empty() {
            out.push_str(&format!("\n> Инструменты: {}\n", tools.join(", ")));
        }
        if !m.attachments.is_empty() {
            let names: Vec<&str> = m.attachments.iter().map(|a| a.name.as_str()).collect();
            out.push_str(&format!("\n> Файлы: {}\n", names.join(", ")));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSearchHitDto {
    pub message_id: i64,
    pub role: String,
    pub snippet: String,
    pub created_at: String,
}

// LIKE search inside one chat (the UI also filters client-side; this command
// covers long transcripts). SQLite LIKE folds case only for ASCII, but the
// snippet window is cut around the first case-insensitive match found in Rust,
// which does fold Cyrillic.
pub fn search_messages(
    conn: &Connection,
    chat_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<ChatSearchHitDto>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let mut stmt = conn.prepare(
        "SELECT id, role, content, created_at FROM messages
         WHERE chat_id = ?1 AND content IS NOT NULL
           AND content LIKE '%' || ?2 || '%' ESCAPE '\\'
         ORDER BY position LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![chat_id, escaped, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for row in rows {
        let (message_id, role, content, created_at) = row?;
        out.push(ChatSearchHitDto {
            message_id,
            role,
            snippet: make_snippet(&content, &needle),
            created_at,
        });
    }
    Ok(out)
}

// ±60 chars around the first case-insensitive match, ellipsised at the cuts;
// cuts snap to char boundaries so a multibyte letter is never split.
fn make_snippet(content: &str, needle_lower: &str) -> String {
    const WINDOW: usize = 60;
    let lower = content.to_lowercase();
    let at = lower.find(needle_lower).unwrap_or(0);
    let match_end = at + needle_lower.len();
    let mut start = at.saturating_sub(WINDOW);
    while start > 0 && !content.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = match_end.saturating_add(WINDOW).min(content.len());
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < content.len() { "…" } else { "" };
    format!("{prefix}{}{suffix}", &content[start..end])
}

// Confirmer that blocks the agent thread on a channel until the UI resolves
// the request via resolve_confirmation; a timeout denies. The `request`
// callback publishes the payload (in production: app.emit). Shared by both
// agents: the run key routes the request to the owning project or session.
pub struct ChannelConfirmer {
    pub run: RunKey,
    pub registry: Arc<AgentRegistry>,
    pub timeout: Duration,
    pub request: Box<dyn Fn(ConfirmationRequestPayload) + Send + Sync>,
}

impl Confirmer for ChannelConfirmer {
    fn confirm(&self, req: &ConfirmRequest) -> ConfirmDecision {
        if self.registry.is_allowed(&req.tool) {
            return ConfirmDecision::AllowAlways;
        }
        let (id, rx) = self.registry.register(&req.tool);
        (self.request)(ConfirmationRequestPayload {
            id,
            project_id: self.run.project_id(),
            session_id: self.run.session_id(),
            notes: self.run.is_notes().then_some(true),
            tool: req.tool.clone(),
            args: req.args.clone(),
            danger_reason: req.danger_reason.clone(),
        });
        let decision = rx.recv_timeout(self.timeout).unwrap_or(ConfirmDecision::Deny);
        self.registry.forget(id);
        decision
    }
}

// Persists the user turn (plus links to already-saved attachment copies,
// SPEC 6.1), then runs the agent loop on a background thread. Returns the
// project chat id; progress arrives as "agent-event" and
// "agent-confirmation" events.
pub fn start_project_run(
    app: &AppHandle,
    state: &AppState,
    registry: &Arc<AgentRegistry>,
    project_id: i64,
    text: &str,
    attachments_in: &[AttachmentRef],
) -> Result<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AppError::Invalid("message text is empty".into()));
    }
    let wd = state.working_dir()?;
    let (chat_id, project, brief, memory_records) = {
        let guard = state.db.lock().unwrap();
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let project = projects::get_project(conn, project_id)?;
        let chat_id = ensure_project_chat(conn, project_id)?;
        // The user turn is persisted first: build_context then sees the full
        // transcript (including it) when estimating and compacting.
        let user_message = append_message(conn, chat_id, Role::User, Some(text), &[], None, None)?;
        attachments::save_message_attachments(conn, user_message.id, attachments_in)?;
        // Cached project brief (brief.rs): read-only here; generation/refresh
        // happens on project open and file changes. The brief and the
        // query-relevant memory records are volatile: they ride the
        // [Context] block of the fresh user turn, never the system prompt.
        let brief = crate::brief::get_brief(conn, project_id)?;
        let memory_records = memory::relevant_records(conn, Some(project_id), text)?;
        (chat_id, project, brief, memory_records)
    };

    // The catalog is built BEFORE the prompt so the generated tool block
    // covers everything the run can see, MCP bridges included (schemas stay
    // lazy via list_available_tools/enable_tools).
    let mut catalog = project_catalog();
    // MCP tools are bridged from the app-level registry: the run's catalog
    // borrows the live server clients (SPEC 6.2).
    register_mcp_tools(&mut catalog, state.mcp.lock().unwrap().tools());
    let skills = crate::skills::short_list(&wd);
    // Stable system prompt only: identical bytes for every turn of the
    // project, whatever the brief or the memory query says.
    let system_prompt = project_system_prompt(&project, &skills, &catalog.summary_prompt_block());
    let brief_section = match brief.as_ref() {
        Some(b) => format!("Project brief (updated {}):\n{}", b.updated_at, b.text),
        None => "Project brief:\nNo brief yet.".to_string(),
    };
    let volatile_block = context::volatile_block(&[&brief_section, &memory_records]);

    spawn_run(
        app,
        &wd,
        registry,
        RunBridge {
            run: RunKey::Project(project_id),
            kind: "project",
            chat_id,
            system_prompt,
            volatile_block,
            catalog,
            notes_root: None,
        },
    );
    Ok(chat_id)
}

// MCP/built-in name collisions stay visible as hidden, not UNKNOWN
// (SPEC 6.4 "collisions are dropped noticeably").
pub(crate) fn register_mcp_tools(catalog: &mut Catalog, tools: Vec<Arc<dyn Tool>>) {
    for tool in tools {
        let name = tool.name().to_string();
        if catalog.register(tool, true).is_err() {
            catalog.hide(&name, "name collision with the built-in catalog");
        }
    }
}

// The pieces that differ between the two agents. Everything else — the run
// thread's own DB connection, config read, memory injection, context build,
// confirmation bridge, cancel wiring, event routing and transcript
// persistence — is shared by run_agent_bridge, so project chats and harness
// sessions run through one loop.
pub(crate) struct RunBridge {
    pub run: RunKey,
    // Journal run kind (SPEC 3.3): "project" | "harness" | "schedule".
    pub kind: &'static str,
    pub chat_id: i64,
    // Stable system prompt: identical bytes across the turns of a chat, so
    // the provider's prompt cache keeps its prefix (system + history).
    pub system_prompt: String,
    // The volatile [Context] block: project brief (with its updated
    // timestamp), memory records relevant to the fresh user turn, and the
    // notes vault summary. build_context prepends it to the last user
    // message of the model-facing view only — the stored row stays plain.
    pub volatile_block: String,
    pub catalog: Catalog,
    // Canonical Notes/ vault root for notes-agent runs (None otherwise):
    // handed to the notes tools through ToolCtx.notes_root.
    pub notes_root: Option<PathBuf>,
}

// Fixed per-agent sampling temperature, applied to every run regardless of
// llm.json (the Settings UI no longer exposes it): notes run a touch warmer,
// project/harness/scheduled runs stay deterministic.
pub(crate) fn temperature_for_kind(kind: &str) -> f32 {
    match kind {
        "notes" => 0.4,
        _ => 0.2,
    }
}

// Spawns the shared agent run on a background thread for either agent.
pub(crate) fn spawn_run(
    app: &AppHandle,
    wd: &Path,
    registry: &Arc<AgentRegistry>,
    bridge: RunBridge,
) {
    let app = app.clone();
    let registry = Arc::clone(registry);
    let wd = wd.to_path_buf();
    std::thread::spawn(move || run_agent_bridge(&app, &registry, &wd, bridge));
}

// Runs the agent loop for a project chat or a harness session, emitting the
// terminal error event (routed by run key) when the run itself fails.
fn run_agent_bridge(
    app: &AppHandle,
    registry: &Arc<AgentRegistry>,
    wd: &Path,
    bridge: RunBridge,
) {
    let (run, chat_id) = (bridge.run, bridge.chat_id);
    if let Err(e) = run_agent_inner(app, registry, wd, bridge) {
        let _ = app.emit(
            "agent-event",
            AgentEventPayload {
                chat_id,
                project_id: run.project_id(),
                session_id: run.session_id(),
                notes: run.is_notes().then_some(true),
                event: agent_loop::AgentEvent::Error {
                    code: "run".into(),
                    message: e.to_string(),
                },
            },
        );
    }
}

fn run_agent_inner(
    app: &AppHandle,
    registry: &Arc<AgentRegistry>,
    wd: &Path,
    bridge: RunBridge,
) -> Result<()> {
    let RunBridge {
        run,
        kind,
        chat_id,
        mut system_prompt,
        volatile_block,
        mut catalog,
        notes_root,
    } = bridge;
    // The run thread owns its DB connection: a long agent turn must not
    // hold the UI connection's mutex; WAL + busy_timeout keeps both safe.
    let thread_state = Arc::new(AppState::new());
    *thread_state.working_dir.lock().unwrap() = Some(wd.to_path_buf());
    *thread_state.db.lock().unwrap() = Some(db::open(&paths::db_path(wd))?);
    // Tools that notify the UI (rename_chat, todowrite) emit through this handle.
    *thread_state.emitter.lock().unwrap() = Some(Arc::new(app.clone()));

    let llm_cfg: LlmConfig = serde_json::from_value(config::read_config(wd, ConfigKind::Llm)?)?;
    // Temperature is fixed per agent (project/harness/schedule 0.2, notes 0.4)
    // and always overrides whatever llm.json carries, so every run path —
    // including scheduled ones — sends the same value.
    let params = ChatParams {
        temperature: Some(temperature_for_kind(kind)),
        ..ChatParams::from(&llm_cfg)
    };
    // Manual `vision` override wins over the cached probe result; unknown
    // means images are not sent and the turn carries an honest note.
    let vision_supported = llm_cfg.vision_enabled();
    let client = llm::client_from_config(wd)?;
    let app_cfg: AppConfig =
        serde_json::from_value(config::read_config(wd, ConfigKind::App)?).unwrap_or_default();

    // Global memory (SPEC 6.3): only the stable part — the fixed identity
    // lines and the always-injected gender line — is appended to the system
    // prompt of every run (project chats and harness sessions alike); it is
    // the same for any query, so the cacheable prefix survives. The records
    // relevant to the fresh user turn already ride the volatile block.
    {
        let guard = thread_state.db.lock().unwrap();
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        system_prompt.push_str(&format!("\n\n{}", memory::identity_block(conn)?));
    }

    // Build the model-facing transcript with checkpoint compaction (SPEC 4.6,
    // 6.4); on summarization failure build_context falls back to full history.
    let ctx = {
        let guard = thread_state.db.lock().unwrap();
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        context::build_context(
            conn,
            chat_id,
            &system_prompt,
            &volatile_block,
            llm_cfg.context_window,
            app_cfg.compaction_threshold_percent,
            &client,
            &params,
            vision_supported,
        )?
    };
    if let Some(up_to_position) = ctx.new_checkpoint_up_to {
        let _ = app.emit(
            "agent-context-compacted",
            json!({ "chatId": chat_id, "upToPosition": up_to_position }),
        );
    }

    let mut agent_state = agent_loop::AgentState {
        backend: &client,
        catalog: &mut catalog,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    registry.set_cancel(run, &cancel);

    let confirmer_app = app.clone();
    let confirmer = ChannelConfirmer {
        run,
        registry: Arc::clone(registry),
        timeout: CONFIRM_TIMEOUT,
        request: Box::new(move |payload| {
            let _ = confirmer_app.emit("agent-confirmation", payload);
        }),
    };
    let config = agent_loop::AgentRunConfig {
        messages: ctx.messages,
        max_steps: MAX_STEPS,
        limits: agent_loop::RunLimits::default(),
        // Full autopilot (SPEC 5.2) short-circuits the confirmation queue.
        allow_all: app_cfg.autopilot,
        project_id: run.project_id(),
        session_id: run.session_id(),
        chat_id: Some(chat_id),
        params,
        cancel: Some(Arc::clone(&cancel)),
        // Tools reach the DB and the working dir through the run's own
        // thread state (fs/task/memory tools borrow this connection).
        state: Some(Arc::clone(&thread_state)),
        working_dir: Some(wd.to_path_buf()),
        notes_root,
    };

    let emit_app = app.clone();
    // Run/tool journal (SPEC 3.3, 6 "все вызовы пишутся в журнал запусков"):
    // every tool call of every run kind is appended to Settings/logs as a
    // masked JSONL line through the app-level journal handle.
    let journal = app.state::<AppState>().journal.lock().unwrap().clone();
    let run_info = crate::journal::RunInfo {
        kind,
        chat_id,
        project_id: run.project_id(),
        session_id: run.session_id(),
    };
    let mut tool_seq: usize = 0;
    let mut tool_started: Option<(std::time::Instant, Value)> = None;
    let mut emit = move |event: agent_loop::AgentEvent| {
        if let Some(journal) = &journal {
            match &event {
                agent_loop::AgentEvent::ToolCallStarted { args, .. } => {
                    tool_started = Some((std::time::Instant::now(), args.clone()));
                }
                agent_loop::AgentEvent::ToolCallFinished { name, ok, envelope } => {
                    tool_seq += 1;
                    let (ms, args) = match tool_started.take() {
                        Some((started_at, args)) => (started_at.elapsed().as_millis(), args),
                        None => (0, Value::Null),
                    };
                    let code = envelope
                        .get("error")
                        .and_then(|e| e.get("code"))
                        .and_then(Value::as_str);
                    let _ = journal.log_event(&run_info, tool_seq, name, *ok, code, ms, &args, envelope);
                }
                _ => {}
            }
        }
        let _ = emit_app.emit(
            "agent-event",
            AgentEventPayload {
                chat_id,
                project_id: run.project_id(),
                session_id: run.session_id(),
                notes: run.is_notes().then_some(true),
                event,
            },
        );
    };
    let persist_state = Arc::clone(&thread_state);
    let mut on_message = move |msg: &ChatMessage| {
        let guard = persist_state.db.lock().unwrap();
        if let Some(conn) = guard.as_ref() {
            let _ = append_chat_message(conn, chat_id, msg);
        }
    };

    let result = agent_loop::run_with(&mut agent_state, &config, &confirmer, &mut emit, &mut on_message);
    registry.clear_cancel(run);
    let result = result?;
    let guard = thread_state.db.lock().unwrap();
    if let Some(conn) = guard.as_ref() {
        // The chat may be gone by the end of the run: the agent deleted its
        // own session (delete_chat) or the user removed it mid-run. The
        // transcript appends above already failed FK-safe and were dropped;
        // skip the final answer the same way instead of surfacing a run
        // error for a chat nobody can open anymore.
        let chat_exists = conn
            .query_row(
                "SELECT count(*) FROM chats WHERE id = ?1",
                params![chat_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|rows| rows > 0)
            .unwrap_or(false);
        if chat_exists {
            persist_final_answer(conn, chat_id, &result)?;
        }
    }
    Ok(())
}

// The finish_task answer lives in the tool call arguments, not in the
// transcript, so the final assistant turn is appended after the run. A
// step_limit run already persisted its fallback text through the hook.
pub(crate) fn persist_final_answer(
    conn: &Connection,
    chat_id: i64,
    result: &agent_loop::AgentRunResult,
) -> Result<()> {
    if result.finish_reason != "step_limit" && !result.answer.trim().is_empty() {
        append_full(
            conn,
            chat_id,
            Role::Assistant,
            Some(&result.answer),
            &[],
            None,
            None,
            result.reasoning.as_deref(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Catalog;
    use crate::agent::finish::FinishTaskTool;
    use crate::agent::r#loop::ChatBackend;
    use crate::agent::permissions::{AutoConfirmer, DenyAllConfirmer};
    use crate::agent::tool::{EchoTool, PermissionPolicy};
    use crate::db::schema;
    use crate::llm::{ChatEvent, ChatOutcome, ChatParams, ToolCall, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[test]
    fn run_temperature_is_fixed_per_agent_kind() {
        assert_eq!(temperature_for_kind("project"), 0.2);
        assert_eq!(temperature_for_kind("harness"), 0.2);
        assert_eq!(temperature_for_kind("schedule"), 0.2);
        assert_eq!(temperature_for_kind("notes"), 0.4);

        // Whatever the kind, the fixed value actually reaches the request body.
        for kind in ["project", "harness", "schedule", "notes"] {
            let params = ChatParams {
                model: "m".into(),
                temperature: Some(temperature_for_kind(kind)),
                max_tokens: None,
                reasoning_effort: None,
            };
            let body = crate::llm::build_body(&[ChatMessage::user("hi")], None, &params, false);
            assert_eq!(body["temperature"], json!(temperature_for_kind(kind)));
        }
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

    fn text_outcome(content: &str) -> ChatOutcome {
        ChatOutcome {
            content: Some(content.into()),
            reasoning: None,
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
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

    fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall::new(id.into(), name.into(), args.to_string())
    }

    fn finish_call(answer: &str) -> ToolCall {
        call("f1", "finish_task", json!({ "action": "finish", "answer": answer }))
    }

    fn setup(tag: &str) -> (std::path::PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        (wd, conn, dto.id)
    }

    fn test_config(user_message: &str) -> agent_loop::AgentRunConfig {
        agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user(user_message)],
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
            working_dir: None,
            notes_root: None,
        }
    }

    fn catalog_with(extra: Option<EchoTool>) -> Catalog {
        let mut c = Catalog::new();
        c.register(Arc::new(FinishTaskTool), true).unwrap();
        if let Some(tool) = extra {
            c.register(Arc::new(tool), true).unwrap();
        }
        c
    }

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
    fn history_round_trips_tool_calls_in_order() {
        let (wd, conn, pid) = setup("chat-history");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("hello"), &[], None, None).unwrap();
        let calls = vec![call("c1", "read_file", json!({ "rel_path": "a.md" }))];
        append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        append_message(
            &conn,
            chat_id,
            Role::Tool,
            Some("{\"ok\":true}"),
            &[],
            Some("c1"),
            None,
        )
        .unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("done"), &[], None, None).unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 4);
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[1].tool_calls.len(), 1);
        assert_eq!(hist[1].tool_calls[0].id, "c1");
        assert_eq!(hist[1].tool_calls[0].name, "read_file");
        assert_eq!(hist[1].tool_calls[0].raw_arguments, "{\"rel_path\":\"a.md\"}");
        assert_eq!(hist[2].role, Role::Tool);
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(hist[3].text_content(), Some("done"));

        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos.len(), 4);
        assert_eq!(dtos[0].role, "user");
        assert_eq!(dtos[1].tool_calls[0]["name"], "read_file");
        assert_eq!(dtos[2].tool_call_id.as_deref(), Some("c1"));
        assert!(dtos[0].position < dtos[1].position);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn reasoning_round_trips_through_the_db_back_into_the_model_transcript() {
        let (wd, conn, pid) = setup("chat-reasoning");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();
        let asst = ChatMessage::assistant("done").with_reasoning(Some("because".into()));
        append_chat_message(&conn, chat_id, &asst).unwrap();

        // The UI DTO carries the trace so the transcript can show it.
        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos[0].reasoning, None);
        assert_eq!(dtos[1].reasoning.as_deref(), Some("because"));

        // The model-facing transcript replays it as reasoning_content.
        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist[1].reasoning_content.as_deref(), Some("because"));
        let wire = serde_json::to_value(&hist).unwrap();
        assert_eq!(wire[1]["reasoning_content"], "because");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn send_persists_attachments_linked_to_user_message_and_history_returns_them() {
        let (wd, conn, pid) = setup("chat-attach-history");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("проверь счёт"), &[], None, None)
            .unwrap();
        let refs = vec![
            AttachmentRef {
                name: "счёт.docx".into(),
                saved_path: "Входящие/счёт.docx".into(),
                kind: "document".into(),
                is_image: false,
            },
            AttachmentRef {
                name: "pic.png".into(),
                saved_path: "Входящие/pic.png".into(),
                kind: "image".into(),
                is_image: true,
            },
        ];
        attachments::save_message_attachments(&conn, user.id, &refs).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("ок"), &[], None, None).unwrap();

        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos.len(), 2);
        assert_eq!(dtos[0].attachments.len(), 2);
        assert_eq!(dtos[0].attachments[0].name, "счёт.docx");
        assert_eq!(dtos[0].attachments[0].saved_path, "Входящие/счёт.docx");
        assert_eq!(dtos[0].attachments[0].kind, "document");
        assert!(dtos[0].attachments[1].is_image);
        assert!(dtos[1].attachments.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_expands_document_text_and_notes_images_when_vision_off() {
        let (wd, conn, pid) = setup("chat-attach-turn");
        let inbox = paths::projects_dir(&wd).join("proj").join("Входящие");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "данные проекта").unwrap();
        fs::write(inbox.join("pic.png"), crate::llm::TINY_PNG).unwrap();

        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("что тут?"), &[], None, None)
            .unwrap();
        attachments::save_message_attachments(
            &conn,
            user.id,
            &[
                AttachmentRef {
                    name: "a.md".into(),
                    saved_path: "Входящие/a.md".into(),
                    kind: "document".into(),
                    is_image: false,
                },
                AttachmentRef {
                    name: "pic.png".into(),
                    saved_path: "Входящие/pic.png".into(),
                    kind: "image".into(),
                    is_image: true,
                },
            ],
        )
        .unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        let turn = hist[0].text_content().unwrap();
        assert!(turn.starts_with("что тут?"));
        // Documents contribute their extracted text to the model turn...
        assert!(turn.contains("[Attachment: a.md]\nданные проекта"));
        // ...images are honestly noted and not sent (no base64 in the turn).
        assert!(turn.contains("[Image \"pic.png\" not sent: the model does not support vision]"));
        assert!(!turn.contains("base64"));

        // The stored row keeps the plain user text: the UI renders the
        // attachments as separate chips next to the bubble.
        let raw: String = conn
            .query_row("SELECT content FROM messages WHERE id = ?1", params![user.id], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "что тут?");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn load_history_sends_image_parts_when_vision_on() {
        let (wd, conn, pid) = setup("chat-attach-turn-vision");
        let inbox = paths::projects_dir(&wd).join("proj").join("Входящие");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("a.md"), "данные проекта").unwrap();
        fs::write(inbox.join("pic.png"), crate::llm::TINY_PNG).unwrap();

        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let user = append_message(&conn, chat_id, Role::User, Some("что тут?"), &[], None, None)
            .unwrap();
        attachments::save_message_attachments(
            &conn,
            user.id,
            &[
                AttachmentRef {
                    name: "a.md".into(),
                    saved_path: "Входящие/a.md".into(),
                    kind: "document".into(),
                    is_image: false,
                },
                AttachmentRef {
                    name: "pic.png".into(),
                    saved_path: "Входящие/pic.png".into(),
                    kind: "image".into(),
                    is_image: true,
                },
            ],
        )
        .unwrap();

        let hist = load_history(&conn, chat_id, true).unwrap();
        // With vision on, the user turn is a parts array: text + image_url.
        let wire = serde_json::to_value(&hist[0]).unwrap();
        let parts = wire["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"]
            .as_str()
            .unwrap()
            .contains("[Attachment: a.md]\nданные проекта"));
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        // No honest "not sent" note when the image is actually sent.
        assert!(!serde_json::to_string(&hist[0]).unwrap().contains("not sent"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn scripted_run_persists_transcript_and_emits_events_in_order() {
        let (wd, conn, pid) = setup("chat-scripted-run");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();

        let mut thinking_round = tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]);
        thinking_round.reasoning = Some("planning the echo".into());
        let backend = ScriptedBackend::new(vec![
            thinking_round,
            tools_outcome(vec![finish_call("final answer")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Skip)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let mut on_message = |msg: &ChatMessage| {
            append_chat_message(&conn, chat_id, msg).unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &DenyAllConfirmer,
            |e| events.push(e),
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();

        // Events in loop order: tool started/finished, finish_task, Done.
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
                if answer == "final answer" && reason == "finish"
        ));

        // DB mirrors the transcript: user, asst(tool_call), tool, asst(finish), tool, asst(answer).
        let hist = load_history(&conn, chat_id, false).unwrap();
        let roles: Vec<Role> = hist.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(hist[1].tool_calls[0].name, "echo");
        assert_eq!(hist[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(hist[3].tool_calls[0].name, "finish_task");
        assert_eq!(hist[5].text_content(), Some("final answer"));
        // Thinking traces persist with their turns: the tool-call turn keeps
        // its own trace, and the finish_task answer turn carries the trace
        // accumulated over the run, so a follow-up request can replay it.
        assert_eq!(hist[1].reasoning_content.as_deref(), Some("planning the echo"));
        assert_eq!(hist[2].reasoning_content, None);
        assert_eq!(hist[5].reasoning_content.as_deref(), Some("planning the echo"));
        let dtos = chat_history(&conn, pid).unwrap();
        assert_eq!(dtos[1].reasoning.as_deref(), Some("planning the echo"));
        assert_eq!(dtos[5].reasoning.as_deref(), Some("planning the echo"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn step_limit_does_not_duplicate_the_fallback_answer() {
        let (wd, conn, pid) = setup("chat-step-limit");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let backend = ScriptedBackend::new(vec![text_outcome("thought")]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.max_steps = 1;
        let mut on_message = |msg: &ChatMessage| {
            append_message(&conn, chat_id, msg.role, msg.text_content(), &msg.tool_calls, None, None)
                .unwrap();
        };
        let result = agent_loop::run_with(
            &mut state,
            &cfg,
            &DenyAllConfirmer,
            |_| {},
            &mut on_message,
        )
        .unwrap();
        persist_final_answer(&conn, chat_id, &result).unwrap();
        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].text_content(), Some("thought"));
        fs::remove_dir_all(&wd).unwrap();
    }

    fn confirmer_with(
        registry: &Arc<AgentRegistry>,
        payloads: &Arc<Mutex<Vec<ConfirmationRequestPayload>>>,
        timeout: Duration,
    ) -> ChannelConfirmer {
        let sink = Arc::clone(payloads);
        ChannelConfirmer {
            run: RunKey::Project(1),
            registry: Arc::clone(registry),
            timeout,
            request: Box::new(move |p| sink.lock().unwrap().push(p)),
        }
    }

    fn spawn_resolver(
        registry: Arc<AgentRegistry>,
        payloads: Arc<Mutex<Vec<ConfirmationRequestPayload>>>,
        decision: ConfirmDecision,
    ) -> std::thread::JoinHandle<u64> {
        std::thread::spawn(move || {
            for _ in 0..1000 {
                if let Some(p) = payloads.lock().unwrap().first().cloned() {
                    registry.resolve(p.id, decision).unwrap();
                    return p.id;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("no confirmation request arrived");
        })
    }

    fn envelope_for(events: &[agent_loop::AgentEvent], name: &str) -> Value {
        events
            .iter()
            .find_map(|e| match e {
                agent_loop::AgentEvent::ToolCallFinished { name: n, envelope, .. } if n == name => {
                    Some(envelope.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no ToolCallFinished for {name}"))
    }

    #[test]
    fn channel_confirmer_allow_lets_the_tool_run() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));
        let resolver = spawn_resolver(Arc::clone(&registry), Arc::clone(&payloads), ConfirmDecision::Allow);

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("ok")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        resolver.join().unwrap();
        assert_eq!(result.answer, "ok");
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
        let p = payloads.lock().unwrap()[0].clone();
        assert_eq!(p.tool, "echo");
        assert_eq!(p.project_id, Some(1));
        assert_eq!(p.session_id, None);
        assert_eq!(p.args, json!({ "text": "hi" }));
    }

    #[test]
    fn channel_confirmer_deny_yields_denied_by_user() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));
        let resolver = spawn_resolver(Arc::clone(&registry), Arc::clone(&payloads), ConfirmDecision::Deny);

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("recovered")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        resolver.join().unwrap();
        assert_eq!(result.answer, "recovered");
        assert_eq!(envelope_for(&events, "echo")["error"]["code"], "denied_by_user");
    }

    #[test]
    fn allow_always_skips_the_next_confirmation_for_that_tool() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));
        let resolver = spawn_resolver(
            Arc::clone(&registry),
            Arc::clone(&payloads),
            ConfirmDecision::AllowAlways,
        );

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "a" }))]),
            tools_outcome(vec![call("c2", "echo", json!({ "text": "b" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &test_config("go"),
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        resolver.join().unwrap();
        assert_eq!(result.answer, "done");
        // Only the first call produced a UI request; the second was answered
        // from the session allow-always set without emitting.
        assert_eq!(payloads.lock().unwrap().len(), 1);
        assert!(registry.is_allowed("echo"));
        let finished: Vec<&Value> = events
            .iter()
            .filter_map(|e| match e {
                agent_loop::AgentEvent::ToolCallFinished { name, envelope, .. } if name == "echo" => {
                    Some(envelope)
                }
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 2);
        assert!(finished.iter().all(|e| e["ok"] == json!(true)));
    }

    #[test]
    fn confirmation_timeout_denies() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        // Nobody resolves: the receiver times out and the decision is Deny.
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_millis(50));
        let req = ConfirmRequest {
            tool: "echo".into(),
            args: json!({ "text": "x" }),
            project_id: Some(1),
            danger_reason: None,
        };
        assert_eq!(confirmer.confirm(&req), ConfirmDecision::Deny);
        assert_eq!(payloads.lock().unwrap().len(), 1);
        // The pending entry was dropped after the timeout.
        assert_eq!(registry.pending_count(), 0);
    }

    #[test]
    fn autopilot_bypasses_the_confirmation_bridge() {
        let registry = Arc::new(AgentRegistry::new());
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let confirmer = confirmer_with(&registry, &payloads, Duration::from_secs(5));

        let backend = ScriptedBackend::new(vec![
            tools_outcome(vec![call("c1", "echo", json!({ "text": "hi" }))]),
            tools_outcome(vec![finish_call("done")]),
        ]);
        let mut catalog = catalog_with(Some(EchoTool::new(PermissionPolicy::Check)));
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.allow_all = true;
        let mut events = Vec::new();
        let result = agent_loop::run_with(
            &mut state,
            &cfg,
            &confirmer,
            |e| events.push(e),
            |_| {},
        )
        .unwrap();
        assert_eq!(result.answer, "done");
        assert!(payloads.lock().unwrap().is_empty());
        assert!(envelope_for(&events, "echo").get("ok").unwrap().as_bool().unwrap());
    }

    #[test]
    fn resolve_confirmation_rejects_unknown_ids() {
        let registry = AgentRegistry::new();
        assert!(registry.resolve(999, ConfirmDecision::Allow).is_err());
    }

    #[test]
    fn event_payload_routes_by_run_key() {
        let ev = agent_loop::AgentEvent::Done {
            answer: "x".into(),
            reason: "finish".into(),
            reasoning: None,
        };
        let project = serde_json::to_value(AgentEventPayload {
            chat_id: 7,
            project_id: RunKey::Project(3).project_id(),
            session_id: RunKey::Project(3).session_id(),
            notes: RunKey::Project(3).is_notes().then_some(true),
            event: ev.clone(),
        })
        .unwrap();
        assert_eq!(project["chatId"], 7);
        assert_eq!(project["projectId"], 3);
        assert!(project.get("sessionId").is_none(), "project run leaked sessionId");
        assert!(project.get("notes").is_none(), "project run leaked notes flag");

        let session = serde_json::to_value(AgentEventPayload {
            chat_id: 8,
            project_id: RunKey::Session(4).project_id(),
            session_id: RunKey::Session(4).session_id(),
            notes: RunKey::Session(4).is_notes().then_some(true),
            event: ev.clone(),
        })
        .unwrap();
        assert_eq!(session["sessionId"], 4);
        assert!(session.get("projectId").is_none(), "session run leaked projectId");
        assert!(session.get("notes").is_none(), "session run leaked notes flag");

        let notes = serde_json::to_value(AgentEventPayload {
            chat_id: 9,
            project_id: RunKey::Notes.project_id(),
            session_id: RunKey::Notes.session_id(),
            notes: RunKey::Notes.is_notes().then_some(true),
            event: ev,
        })
        .unwrap();
        assert_eq!(notes["chatId"], 9);
        assert_eq!(notes["notes"], true);
        assert!(notes.get("projectId").is_none(), "notes run leaked projectId");
        assert!(notes.get("sessionId").is_none(), "notes run leaked sessionId");
    }

    #[test]
    fn cancel_flag_stops_a_run_and_keeps_completed_steps_persisted() {
        let (wd, conn, pid) = setup("chat-cancel");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let backend = ScriptedBackend::new(vec![tools_outcome(vec![finish_call("never")])]);
        let mut catalog = catalog_with(None);
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let mut cfg = test_config("go");
        cfg.cancel = Some(Arc::clone(&cancel));
        let result = agent_loop::run_with(&mut state, &cfg, &AutoConfirmer::allowing(true), |_| {}, |_| {})
            .unwrap();
        assert_eq!(result.finish_reason, "cancelled");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        persist_final_answer(&conn, chat_id, &result).unwrap();
        assert!(load_history(&conn, chat_id, false).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn harness_message_appends_mirror_into_messages_fts() {
        let (wd, conn, pid) = setup("chat-fts-sync");
        let session = crate::harness::create_session(&conn, Some("s")).unwrap();
        let chat_id = crate::harness::session_chat_id(&conn, session.id).unwrap();
        append_message(&conn, chat_id, Role::User, Some("ищем редизайн главной"), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("   "), &[], None, None).unwrap();
        append_message(&conn, chat_id, Role::Tool, Some("редизайн tool"), &[], None, None).unwrap();
        let project_chat = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, project_chat, Role::User, Some("редизайн проекта"), &[], None, None).unwrap();

        let mut stmt = conn
            .prepare("SELECT message_id, session_id, content FROM messages_fts WHERE messages_fts MATCH ?1")
            .unwrap();
        let rows: Vec<(i64, i64, String)> = stmt
            .query_map(params!["\"редизайн\""], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "only the harness user turn must be findable");
        assert_eq!(rows[0].1, session.id);
        assert_eq!(rows[0].2, "ищем редизайн главной");
        // Blank assistant turns, tool turns and project chats never enter the index.
        let total: i64 = conn
            .query_row("SELECT count(*) FROM messages_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    // Deterministic stand-in: byte-bucket counts, no network.
    struct FakeEmbedder;

    impl embed::Embedder for FakeEmbedder {
        fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 768];
                    for b in t.bytes() {
                        v[b as usize % 768] += 1.0;
                    }
                    v
                })
                .collect())
        }
        fn dim(&self) -> usize {
            embed::EMBED_DIM
        }
        fn backend_id(&self) -> &'static str {
            "fake"
        }
    }

    #[test]
    fn index_chat_messages_backfills_vectors_and_skips_noop() {
        let (wd, conn, _pid) = setup("chat-vec-backfill");
        let session = crate::harness::create_session(&conn, Some("s")).unwrap();
        let chat_id = crate::harness::session_chat_id(&conn, session.id).unwrap();
        // Direct SQL inserts: no vectors and no FTS rows (append path bypassed).
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (?1, 'user', 'кошки кошки кошки', '2026-01-01', 1)",
            params![chat_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (?1, 'tool', 'кошки tool не индексируется', '2026-01-01', 2)",
            params![chat_id],
        )
        .unwrap();

        assert_eq!(index_chat_messages_with(&conn, &embed::NoopEmbedder).unwrap(), 0);
        assert_eq!(index_chat_messages_with(&conn, &FakeEmbedder).unwrap(), 1);
        let vectors: i64 = conn
            .query_row("SELECT count(*) FROM message_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vectors, 1);
        let rowid: i64 = conn
            .query_row("SELECT rowid FROM vec_messages", [], |r| r.get(0))
            .unwrap();
        let message_id: i64 = conn
            .query_row("SELECT id FROM messages WHERE role = 'user'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rowid, message_id, "vector rowid must equal the message id");
        // Idempotent: a second pass stores nothing new.
        assert_eq!(index_chat_messages_with(&conn, &FakeEmbedder).unwrap(), 0);
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

    #[test]
    fn export_markdown_keeps_roles_and_skips_tool_noise() {
        let (wd, conn, pid) = setup("chat-export");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("сделай отчёт"), &[], None, None).unwrap();
        let calls = vec![call("c1", "read_file", json!({ "rel_path": "a.md" }))];
        append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        append_message(&conn, chat_id, Role::Tool, Some("{\"ok\":true}"), &[], Some("c1"), None).unwrap();
        let asst = ChatMessage::assistant("готово").with_reasoning(Some("думал".into()));
        append_chat_message(&conn, chat_id, &asst).unwrap();

        let md = export_markdown(&conn, chat_id).unwrap();
        assert!(md.contains("## Вы ·"));
        assert!(md.contains("сделай отчёт"));
        assert!(md.contains("## Агент ·"));
        assert!(md.contains("готово"));
        assert!(md.contains("<details><summary>Размышления</summary>"));
        assert!(md.contains("думал"));
        assert!(md.contains("> Инструменты: read_file"));
        assert!(!md.contains("{\"ok\":true}"), "tool result noise must not leak into the export");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn search_messages_finds_cyrillic_inside_one_chat() {
        let (wd, conn, pid) = setup("chat-search");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(
            &conn,
            chat_id,
            Role::User,
            Some("подготовь коммерческое предложение для example.ru"),
            &[],
            None,
            None,
        )
        .unwrap();
        append_message(&conn, chat_id, Role::Assistant, Some("собрал предложение, файл во вложениях"), &[], None, None).unwrap();

        let hits = search_messages(&conn, chat_id, "предлож", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].snippet.contains("предлож"));

        // Blank queries search nothing; the scope stays inside the chat.
        assert!(search_messages(&conn, chat_id, "   ", 10).unwrap().is_empty());
        let session = crate::harness::create_session(&conn, Some("s")).unwrap();
        let other_chat = crate::harness::session_chat_id(&conn, session.id).unwrap();
        append_message(&conn, other_chat, Role::User, Some("предложение в другом чате"), &[], None, None).unwrap();
        assert_eq!(search_messages(&conn, chat_id, "предлож", 10).unwrap().len(), 2);
        fs::remove_dir_all(&wd).unwrap();
    }
}
