use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};

// Chat-level digest for a search hit: the latest checkpoint summary when the
// chat was compacted, else the first user message. Whitespace is collapsed and
// the result is truncated on a char boundary; no LLM call.
pub(crate) const CHAT_DIGEST_MAX_CHARS: usize = 160;

pub(crate) fn chat_digest(conn: &Connection, session_id: i64) -> String {
    let summary: Option<String> = conn
        .query_row(
            "SELECT s.summary FROM summaries s
             JOIN chats c ON c.id = s.chat_id
             WHERE c.session_id = ?1 AND c.kind = 'harness'
             ORDER BY s.up_to_position DESC
             LIMIT 1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    let raw = match summary.filter(|s| !s.trim().is_empty()) {
        Some(s) => s,
        None => conn
            .query_row(
                "SELECT m.content FROM messages m
                 JOIN chats c ON c.id = m.chat_id
                 WHERE c.session_id = ?1 AND c.kind = 'harness' AND m.role = 'user'
                 ORDER BY m.position ASC
                 LIMIT 1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or_default(),
    };
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flat, CHAT_DIGEST_MAX_CHARS)
}

pub(crate) fn session_info(conn: &Connection, session_id: i64) -> Result<Option<(Option<String>, String)>> {
    Ok(conn
        .query_row(
            "SELECT title, last_active_at FROM sessions WHERE id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub(crate) fn message_content(conn: &Connection, message_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT content FROM messages WHERE id = ?1",
        params![message_id],
        |r| r.get(0),
    )
    .ok()
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}
