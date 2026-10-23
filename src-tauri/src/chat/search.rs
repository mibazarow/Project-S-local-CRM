// Chat search mirrors and queries (schema v19): harness user/assistant turns
// are mirrored into messages_fts on append and embedded best-effort into
// vec_messages; the backfill catches transcripts stored before their
// embeddings existed; the LIKE search covers long transcripts in one chat.
use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::llm::Role;
use crate::paths;
use crate::state::AppState;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use super::types::ChatSearchHitDto;

// Chat search index (schema v19): harness user/assistant turns are mirrored
// into messages_fts on append and embedded best-effort into vec_messages on a
// background thread. Project and notes chats stay out of scope (harness chat
// search only); tool turns and blank content are never indexed.
pub(crate) fn sync_harness_message_index(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_support::setup;
    use crate::chat::{append_message, ensure_project_chat};
    use std::fs;

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
