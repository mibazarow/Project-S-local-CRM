// Chat persistence (SPEC 3.3, 4.6): CRUD, model-facing history, the message
// search mirrors (FTS + vectors) and the message-level chat UX. The run
// orchestration — send_project_message persisting the user turn, the agent
// loop on a background thread with its own DB connection (WAL-safe, same
// pattern as index/search), the "agent-event" / "agent-confirmation" streams
// and the transcript mirroring through the loop's on_message hook — lives in
// the `run` submodule; its items are re-exported here so call sites keep the
// `chat::…` paths. The DB copy can never diverge from what the model saw,
// and the transcript stays append-only.
pub mod context;
mod run;

pub use run::start_project_run;
pub(crate) use run::{register_mcp_tools, RunBridge, spawn_run};
// Only the harness/notes test call sites use this directly; production run
// paths reach it inside spawn_run's thread.
#[cfg(test)]
pub(crate) use run::persist_final_answer;

use crate::attachments::{self, AttachmentRef};
use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::llm::{sanitize_tool_history, ChatMessage, ContentPart, MessageContent, Role, ToolCall};
use crate::paths;
use crate::state::AppState;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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
    // The model-facing view is repaired on load: truncation, regeneration,
    // branching, compaction or a run killed mid-tool can leave an assistant
    // turn with tool_calls whose tool results are gone (HTTP 400 at send
    // time). The stored rows stay append-only and untouched; every run path
    // (project/harness/notes/schedule) builds its context through here.
    Ok(sanitize_tool_history(&out))
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

// Shared test scaffolding for this module and `run`: an in-memory DB with
// migrations plus a sample project, and the tool-call literal.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{Connection, ToolCall, Value};
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
}

#[cfg(test)]
mod tests {
    use super::test_support::{call, setup};
    use super::*;
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

    // Wire-level check: every assistant tool_calls turn is followed by a tool
    // message per tool_call_id before the next assistant turn (or the end).
    fn tool_groups_are_closed(messages: &[ChatMessage]) -> bool {
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

    #[test]
    fn load_history_closes_open_tool_calls_with_synthetic_results() {
        let (wd, conn, pid) = setup("chat-sanitize-load");
        let chat_id = ensure_project_chat(&conn, pid).unwrap();
        append_message(&conn, chat_id, Role::User, Some("go"), &[], None, None).unwrap();
        let calls = vec![
            call("c1", "read_file", json!({ "rel_path": "a.md" })),
            call("c2", "read_file", json!({ "rel_path": "b.md" })),
        ];
        append_message(&conn, chat_id, Role::Assistant, None, &calls, None, None).unwrap();
        // Only c1's result was persisted (a killed run lost c2's).
        append_message(&conn, chat_id, Role::Tool, Some("{\"ok\":true}"), &[], Some("c1"), None)
            .unwrap();

        let hist = load_history(&conn, chat_id, false).unwrap();
        assert_eq!(hist.len(), 4, "the synthetic tool message closes the group");
        assert_eq!(hist[3].role, Role::Tool);
        assert_eq!(hist[3].tool_call_id.as_deref(), Some("c2"));
        let envelope: Value = serde_json::from_str(hist[3].text_content().unwrap()).unwrap();
        assert_eq!(envelope["ok"], json!(false));
        assert_eq!(envelope["error"]["code"], "cancelled");
        assert_eq!(envelope["error"]["message"], "tool result missing");
        assert!(tool_groups_are_closed(&hist));
        // The stored rows stay untouched: the repair is model-facing only.
        let stored: i64 = conn
            .query_row("SELECT count(*) FROM messages WHERE chat_id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 3);
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
