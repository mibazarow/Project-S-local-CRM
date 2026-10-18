use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::index;
use crate::paths;
use crate::state::AppState;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashMap;

// Reciprocal rank fusion: each leg contributes 1 / (RRF_K + rank), rank starts at 1.
const RRF_K: f64 = 60.0;
const SNIPPET_WIDTH: usize = 200;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub rel_path: String,
    pub name: String,
    // "file" or "task"; task hits carry an empty rel_path.
    pub kind: String,
    pub score: f64,
    pub snippet: String,
    pub match_types: Vec<String>,
    // File mtime in epoch milliseconds, from indexed_files (0 for task hits).
    pub modified: i64,
    // Task hits (SPEC 4.4): identify the task row for the UI.
    pub task_id: Option<i64>,
    pub status: Option<String>,
    pub overdue: Option<bool>,
}

// Hybrid per-project search: FTS5 keyword leg plus the vector semantic leg
// over files and tasks (closed tasks included), merged with RRF. An empty or
// punctuation-only query yields an empty list.
pub fn search_project(
    state: &AppState,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let embedder = embed::select_embedder(&wd);
    search_with(
        &conn,
        embedder.as_ref(),
        embed::NAME_WEIGHT,
        project_id,
        query,
        limit,
    )
}

// Notes vault search: the same hybrid pipeline scoped to the notes sentinel
// id. The task legs find nothing for it (no tasks carry project_id = -1), so
// results are note files only.
pub fn search_notes(state: &AppState, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
    search_project(state, crate::index::NOTES_SCOPE_ID, query, limit)
}

// One connection serves the whole call: every leg below takes `&Connection`,
// no leg opens its own (S1.2). pub(crate) so crm_search can reuse a single
// connection across projects.
pub(crate) fn search_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    name_weight: f32,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let terms = tokenize(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let match_query = fts_match_query(&terms);
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let fts_rows = fts_search(conn, project_id, &match_query, limit)?;
    let task_rows = task_fts_search(conn, project_id, &match_query, limit)?;
    // Noop embedder: skip the semantic legs entirely, FTS-only stays offline.
    let (semantic, task_semantic) = if embedder.backend_id() == "noop" {
        (Vec::new(), Vec::new())
    } else {
        (
            index::semantic_search_with(conn, embedder, name_weight, project_id, query, limit)?,
            task_semantic_hits(conn, embedder, project_id, query, limit)?,
        )
    };

    let mut merged: HashMap<String, SearchHit> = HashMap::new();
    for (rank, (rel, name, snippet)) in fts_rows.into_iter().enumerate() {
        let hit = merged.entry(rel.clone()).or_insert_with(|| SearchHit {
            rel_path: rel,
            name,
            kind: "file".into(),
            score: 0.0,
            snippet,
            match_types: Vec::new(),
            modified: 0,
            task_id: None,
            status: None,
            overdue: None,
        });
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("fts".into());
    }
    for (rank, sem) in semantic.into_iter().enumerate() {
        let hit = merged
            .entry(sem.rel_path.clone())
            .or_insert_with(|| SearchHit {
                rel_path: sem.rel_path,
                name: sem.name,
                kind: "file".into(),
                score: 0.0,
                snippet: String::new(),
                match_types: Vec::new(),
                modified: 0,
                task_id: None,
                status: None,
                overdue: None,
            });
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("semantic".into());
    }
    // Task legs merge into the same RRF ranking, keyed by task id.
    let mut merged_tasks: HashMap<i64, SearchHit> = HashMap::new();
    for (rank, row) in task_rows.into_iter().enumerate() {
        let hit = merged_tasks
            .entry(row.id)
            .or_insert_with(|| task_hit(&row, &lowered));
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("fts".into());
    }
    for (rank, row) in task_semantic.into_iter().enumerate() {
        let hit = merged_tasks
            .entry(row.id)
            .or_insert_with(|| task_hit(&row, &lowered));
        hit.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        hit.match_types.push("semantic".into());
    }

    let mut hits: Vec<SearchHit> = merged.into_values().chain(merged_tasks.into_values()).collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.rel_path.cmp(&b.rel_path))
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    hits.truncate(limit);
    enrich_hits(conn, project_id, &mut hits, &lowered)?;
    Ok(hits)
}

// Batched hit enrichment (S1.3): one mtime query and one content query per
// chunk of file hits instead of up to two single-row queries per hit. Chunked
// to stay clear of SQLite's max-variable-number limit. Task hits (empty
// rel_path) are skipped: the old per-hit lookups found no row for them, so
// they keep modified = 0 and their own snippet. Missing rows keep the old
// fallbacks: mtime 0, empty content.
const ENRICH_CHUNK: usize = 500;

fn enrich_hits(
    conn: &Connection,
    project_id: i64,
    hits: &mut [SearchHit],
    lowered: &[String],
) -> Result<()> {
    let rels: Vec<&str> = hits
        .iter()
        .filter(|h| !h.rel_path.is_empty())
        .map(|h| h.rel_path.as_str())
        .collect();
    if rels.is_empty() {
        return Ok(());
    }
    let mut mtimes: HashMap<String, i64> = HashMap::new();
    for chunk in rels.chunks(ENRICH_CHUNK) {
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        args.push(&project_id);
        args.extend(chunk.iter().map(|r| r as &dyn rusqlite::ToSql));
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT rel_path, mtime FROM indexed_files
             WHERE project_id = ?1 AND rel_path IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (rel, mtime) = row?;
            mtimes.insert(rel, mtime);
        }
    }

    // Content is only needed for hits without an FTS snippet (semantic-only
    // hits and name-column matches), same condition as the old per-hit leg.
    let need_content: Vec<&str> = hits
        .iter()
        .filter(|h| !h.rel_path.is_empty() && h.snippet.is_empty())
        .map(|h| h.rel_path.as_str())
        .collect();
    let mut contents: HashMap<String, Option<String>> = HashMap::new();
    let pid_text = project_id.to_string();
    for chunk in need_content.chunks(ENRICH_CHUNK) {
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
        args.push(&pid_text);
        args.extend(chunk.iter().map(|r| r as &dyn rusqlite::ToSql));
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT rel_path, content FROM files_fts
             WHERE project_id = ?1 AND rel_path IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (rel, content) = row?;
            // or_insert: on duplicate FTS rows the first one wins, matching
            // the old query_row which took the first row of the scan.
            contents.entry(rel).or_insert(content);
        }
    }

    for hit in hits.iter_mut() {
        if hit.rel_path.is_empty() {
            continue;
        }
        hit.modified = mtimes.get(hit.rel_path.as_str()).copied().unwrap_or(0);
        if hit.snippet.is_empty() {
            let content = contents
                .get(hit.rel_path.as_str())
                .and_then(Option::as_deref)
                .unwrap_or("");
            hit.snippet = fallback_snippet(content, lowered);
        }
    }
    Ok(())
}

// Harness chat search: FTS5 over messages_fts plus KNN over vec_messages,
// merged per message with RRF, then aggregated per session. Session score =
// SUM of its matched messages' RRF scores (a session with several matches
// outranks a single-match one); the hit carries the session's best
// (highest-RRF) message and its snippet. Index rows left by deleted sessions
// or messages drop out when the session row is looked up.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatHit {
    pub session_id: i64,
    pub title: Option<String>,
    pub last_active: String,
    pub score: f64,
    pub message_id: i64,
    pub snippet: String,
}

pub fn search_chats(state: &AppState, query: &str, limit: usize) -> Result<Vec<ChatHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let embedder = embed::select_embedder(&wd);
    search_chats_with(&conn, embedder.as_ref(), query, limit)
}

pub(crate) fn search_chats_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<ChatHit>> {
    let terms = tokenize(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    // Session aggregation collapses several messages into one hit, so each
    // leg fetches headroom beyond the final limit.
    let candidates = (limit * 5).clamp(10, 250);
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let fts_rows = chat_fts_search(conn, &fts_match_query(&terms), candidates)?;
    // Noop embedder: skip the semantic leg entirely, FTS-only stays offline.
    let semantic = if embedder.backend_id() == "noop" {
        Vec::new()
    } else {
        chat_semantic_hits(conn, embedder, query, candidates)?
    };

    struct Msg {
        message_id: i64,
        session_id: i64,
        score: f64,
        snippet: String,
    }
    let mut merged: HashMap<i64, Msg> = HashMap::new();
    for (rank, (message_id, session_id, snippet)) in fts_rows.into_iter().enumerate() {
        let msg = merged.entry(message_id).or_insert(Msg {
            message_id,
            session_id,
            score: 0.0,
            snippet,
        });
        msg.score += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, (message_id, session_id)) in semantic.into_iter().enumerate() {
        let msg = merged.entry(message_id).or_insert(Msg {
            message_id,
            session_id,
            score: 0.0,
            snippet: String::new(),
        });
        msg.score += 1.0 / (RRF_K + rank as f64 + 1.0);
    }

    let mut per_session: HashMap<i64, (f64, Msg)> = HashMap::new();
    for msg in merged.into_values() {
        match per_session.get_mut(&msg.session_id) {
            Some((score, best)) => {
                *score += msg.score;
                if msg.score > best.score {
                    *best = msg;
                }
            }
            None => {
                per_session.insert(msg.session_id, (msg.score, msg));
            }
        }
    }

    let mut hits: Vec<ChatHit> = Vec::new();
    for (session_id, (score, best)) in per_session {
        let Some((title, last_active)) = session_info(conn, session_id)? else {
            continue; // stale index row: the session is gone
        };
        let snippet = if best.snippet.is_empty() {
            let content = message_content(conn, best.message_id).unwrap_or_default();
            fallback_snippet(&content, &lowered)
        } else {
            best.snippet
        };
        hits.push(ChatHit {
            session_id,
            title,
            last_active,
            score,
            message_id: best.message_id,
            snippet,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    hits.truncate(limit);
    Ok(hits)
}

fn chat_fts_search(
    conn: &Connection,
    match_query: &str,
    limit: usize,
) -> Result<Vec<(i64, i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, session_id, snippet(messages_fts, 2, '<<', '>>', '…', 12)
         FROM messages_fts
         WHERE messages_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_query, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn chat_semantic_hits(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<(i64, i64)>> {
    let mut query_vector = embedder
        .embed_texts(&[embed::query_prefix(query)])?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Invalid("embedder returned no vector".into()))?;
    if query_vector.len() < embedder.dim() {
        return Err(AppError::Invalid(
            "embedder returned too few dims".into(),
        ));
    }
    embed::truncate_renorm(&mut query_vector, embedder.dim());
    let blob: Vec<u8> = query_vector.iter().flat_map(|x| x.to_le_bytes()).collect();
    // Join back to messages/chats: vectors left by deleted messages or
    // sessions never surface as hits.
    let mut stmt = conn.prepare(
        "SELECT q.rowid, c.session_id
         FROM (SELECT rowid, distance FROM vec_messages WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance) q
         JOIN messages m ON m.id = q.rowid
         JOIN chats c ON c.id = m.chat_id AND c.kind = 'harness'
         WHERE c.session_id IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![blob, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn session_info(conn: &Connection, session_id: i64) -> Result<Option<(Option<String>, String)>> {
    Ok(conn
        .query_row(
            "SELECT title, last_active_at FROM sessions WHERE id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn message_content(conn: &Connection, message_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT content FROM messages WHERE id = ?1",
        params![message_id],
        |r| r.get(0),
    )
    .ok()
}

// Task row joined into search: status/overdue come from tasks, so closed
// tasks are included and status/date filtering stays possible via the join.
#[derive(Debug, Clone)]
struct TaskRow {
    id: i64,
    title: String,
    status: String,
    comment: Option<String>,
    overdue: bool,
}

const TASK_COLS: &str = "tasks.id, tasks.title, tasks.status, tasks.comment, \
     (tasks.due_date IS NOT NULL AND tasks.due_date || ' ' || tasks.due_time || ':00' < datetime('now','localtime') AND tasks.status <> 'done')";

fn task_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: row.get(0)?,
        title: row.get(1)?,
        status: row.get(2)?,
        comment: row.get(3)?,
        overdue: row.get::<_, i64>(4)? != 0,
    })
}

fn task_fts_search(
    conn: &Connection,
    project_id: i64,
    match_query: &str,
    limit: usize,
) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_COLS}
         FROM tasks_fts JOIN tasks ON tasks.id = tasks_fts.rowid
         WHERE tasks_fts MATCH ?1 AND tasks.project_id = ?2
         ORDER BY rank
         LIMIT ?3",
    ))?;
    let rows = stmt.query_map(params![match_query, project_id, limit as i64], task_row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Kanban-scale task counts: embed every task text of the project at query
// time instead of storing task vectors — always fresh, no sync or backfill
// burden, and the Noop default keeps this leg off entirely.
fn task_semantic_hits(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    project_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_COLS} FROM tasks WHERE tasks.project_id = ?1"
    ))?;
    let rows = stmt
        .query_map(params![project_id], task_row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let dim = embedder.dim();
    let mut query_vector = embedder
        .embed_texts(&[embed::query_prefix(query)])?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Invalid("embedder returned no vector".into()))?;
    embed::truncate_renorm(&mut query_vector, dim);
    let docs: Vec<String> = rows
        .iter()
        .map(|t| embed::doc_prefix(&t.title, t.comment.as_deref().unwrap_or("")))
        .collect();
    let mut scored: Vec<(f64, TaskRow)> = rows
        .into_iter()
        .zip(embedder.embed_texts(&docs)?)
        .map(|(row, mut vector)| {
            embed::truncate_renorm(&mut vector, dim);
            // unit vectors: cosine similarity = dot product
            let sim: f32 = query_vector.iter().zip(&vector).map(|(a, b)| a * b).sum();
            (sim as f64, row)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(_, row)| row).collect())
}

// Contentless FTS tables cannot snippet, so the task snippet is built around
// the query terms from the task's own title/comment.
fn task_hit(row: &TaskRow, lowered: &[String]) -> SearchHit {
    let mut text = row.title.clone();
    if let Some(comment) = &row.comment {
        text.push(' ');
        text.push_str(comment);
    }
    SearchHit {
        rel_path: String::new(),
        name: row.title.clone(),
        kind: "task".into(),
        score: 0.0,
        snippet: fallback_snippet(&text, lowered),
        match_types: Vec::new(),
        modified: 0,
        task_id: Some(row.id),
        status: Some(row.status.clone()),
        overdue: Some(row.overdue),
    }
}

// Splits user input into plain word tokens; FTS operators ("*", quotes, NEAR,
// OR) never survive as syntax, only as word characters. Shared with memory.
pub(crate) fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

// Each token becomes a quoted phrase (tokens contain no quotes after
// tokenizing), joined with spaces for implicit AND.
pub(crate) fn fts_match_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fts_search(
    conn: &Connection,
    project_id: i64,
    match_query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT rel_path, name, snippet(files_fts, 3, '<<', '>>', '…', 12)
         FROM files_fts
         WHERE files_fts MATCH ?1 AND project_id = ?2
         ORDER BY rank
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![match_query, project_id.to_string(), limit as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Short window around the first query token found in the stored content, else
// a plain prefix. Used for semantic-only hits and FTS hits whose match was in
// the name column (empty content snippet).
fn fallback_snippet(content: &str, terms_lower: &[String]) -> String {
    let chars: Vec<char> = content.chars().collect();
    let lower = content.to_lowercase();
    let best = terms_lower
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .map(|byte_pos| lower[..byte_pos].chars().count())
        .unwrap_or(0);
    let start = best.saturating_sub(SNIPPET_WIDTH / 3);
    let end = (start + SNIPPET_WIDTH).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out
}

// Global cross-entity search for the ⌘K palette: one FTS leg per entity
// (files across all projects, tasks including closed, the notes vault,
// harness chat messages via search_chats) plus plain substring legs for
// project and session titles (SQLite LIKE is ASCII-only, so Cyrillic
// case-folding happens in Rust; those tables stay small at local scale).
// Legs merge by RRF-style rank scores with a deterministic tiebreak.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalHit {
    // "project" | "file" | "task" | "note" | "chat"
    pub kind: String,
    // Entity id: project/task/session id; files_fts rowid for file/note hits.
    pub id: i64,
    pub title: String,
    pub subtitle: String,
    pub snippet: String,
    pub score: f64,
    pub project_id: Option<i64>,
    pub session_id: Option<i64>,
    pub task_id: Option<i64>,
    pub rel_path: Option<String>,
}

pub fn global_search(state: &AppState, query: &str, limit: usize) -> Result<Vec<GlobalHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let embedder = embed::select_embedder(&wd);
    global_search_with(&conn, embedder.as_ref(), query, limit)
}

pub(crate) fn global_search_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    query: &str,
    limit: usize,
) -> Result<Vec<GlobalHit>> {
    let terms = tokenize(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let match_query = fts_match_query(&terms);
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<GlobalHit> = Vec::new();

    {
        let mut stmt = conn.prepare("SELECT id, name FROM projects ORDER BY position, id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut rank = 0usize;
        for row in rows {
            let (id, name) = row?;
            if !name.to_lowercase().contains(&needle) {
                continue;
            }
            hits.push(GlobalHit {
                kind: "project".into(),
                id,
                title: name,
                subtitle: "Проект".into(),
                snippet: String::new(),
                score: 1.0 / (RRF_K + rank as f64 + 1.0),
                project_id: Some(id),
                session_id: None,
                task_id: None,
                rel_path: None,
            });
            rank += 1;
            if rank >= limit {
                break;
            }
        }
    }

    // Files across all projects; the notes scope surfaces as "note" hits below.
    {
        let mut stmt = conn.prepare(
            "SELECT files_fts.rowid, CAST(files_fts.project_id AS INTEGER), files_fts.rel_path,
                    files_fts.name, snippet(files_fts, 3, '<<', '>>', '…', 12), p.name
             FROM files_fts
             JOIN projects p ON p.id = CAST(files_fts.project_id AS INTEGER)
             WHERE files_fts MATCH ?1 AND CAST(files_fts.project_id AS INTEGER) <> ?2
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![match_query, crate::index::NOTES_SCOPE_ID, limit as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )?;
        for (rank, row) in rows.enumerate() {
            let (rowid, project_id, rel_path, name, snippet, project) = row?;
            hits.push(GlobalHit {
                kind: "file".into(),
                id: rowid,
                title: name,
                subtitle: project,
                snippet,
                score: 1.0 / (RRF_K + rank as f64 + 1.0),
                project_id: Some(project_id),
                session_id: None,
                task_id: None,
                rel_path: Some(rel_path),
            });
        }
    }

    // Tasks across all projects, closed included (join tasks, no status filter).
    {
        let mut stmt = conn.prepare(
            "SELECT tasks.id, tasks.title, tasks.status, tasks.comment,
                    tasks.due_date, tasks.due_time, tasks.project_id, p.name
             FROM tasks_fts
             JOIN tasks ON tasks.id = tasks_fts.rowid
             JOIN projects p ON p.id = tasks.project_id
             WHERE tasks_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![match_query, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        for (rank, row) in rows.enumerate() {
            let (id, title, status, comment, due_date, due_time, project_id, project) = row?;
            let mut text = title.clone();
            if let Some(comment) = &comment {
                text.push(' ');
                text.push_str(comment);
            }
            let mut subtitle = format!("{project} · {}", task_status_label(&status));
            if let Some(due) = due_date {
                subtitle.push_str(&format!(" · до {due} {due_time}"));
            }
            hits.push(GlobalHit {
                kind: "task".into(),
                id,
                title,
                subtitle,
                snippet: fallback_snippet(&text, &lowered),
                score: 1.0 / (RRF_K + rank as f64 + 1.0),
                project_id: Some(project_id),
                session_id: None,
                task_id: Some(id),
                rel_path: None,
            });
        }
    }

    // Notes vault: the same scope search_notes uses (project_id = NOTES_SCOPE_ID).
    {
        let mut stmt = conn.prepare(
            "SELECT rowid, rel_path, name, snippet(files_fts, 3, '<<', '>>', '…', 12)
             FROM files_fts
             WHERE files_fts MATCH ?1 AND project_id = ?2
             ORDER BY rank
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![match_query, crate::index::NOTES_SCOPE_ID.to_string(), limit as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?;
        for (rank, row) in rows.enumerate() {
            let (rowid, rel_path, name, snippet) = row?;
            hits.push(GlobalHit {
                kind: "note".into(),
                id: rowid,
                title: name,
                subtitle: "Заметки и файлы".into(),
                snippet,
                score: 1.0 / (RRF_K + rank as f64 + 1.0),
                project_id: None,
                session_id: None,
                task_id: None,
                rel_path: Some(rel_path),
            });
        }
    }

    // Chats: session title substring plus message hits via search_chats,
    // deduped per session (scores add, the message snippet wins).
    {
        let mut by_session: HashMap<i64, (f64, Option<String>, String)> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, title FROM sessions WHERE title IS NOT NULL ORDER BY last_active_at DESC",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut rank = 0usize;
            for row in rows {
                let (id, title) = row?;
                if !title.to_lowercase().contains(&needle) {
                    continue;
                }
                let entry = by_session
                    .entry(id)
                    .or_insert((0.0, None, String::new()));
                entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
                if entry.1.is_none() {
                    entry.1 = Some(title);
                }
                rank += 1;
            }
        }
        for hit in search_chats_with(conn, embedder, query, limit)? {
            let entry = by_session
                .entry(hit.session_id)
                .or_insert((0.0, None, String::new()));
            entry.0 += hit.score;
            if hit.title.is_some() {
                entry.1 = hit.title;
            }
            entry.2 = hit.snippet;
        }
        for (session_id, (score, title, snippet)) in by_session {
            hits.push(GlobalHit {
                kind: "chat".into(),
                id: session_id,
                title: title.unwrap_or_else(|| "Новая сессия".into()),
                subtitle: "Харнес".into(),
                snippet,
                score,
                project_id: None,
                session_id: Some(session_id),
                task_id: None,
                rel_path: None,
            });
        }
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    Ok(hits)
}

fn task_status_label(status: &str) -> &'static str {
    match status {
        "new" => "Новые",
        "progress" => "В работе",
        "done" => "Готово",
        _ => "Задача",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexingConfig;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::tasks;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn setup(tag: &str) -> (PathBuf, Connection, i64, AppState) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        (wd, conn, dto.id, state)
    }

    fn root(wd: &Path) -> PathBuf {
        paths::projects_dir(wd).join("proj")
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

    fn rels(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.rel_path.clone()).collect()
    }

    #[test]
    fn fts_only_finds_cyrillic_word_and_snippet_wraps_match() {
        let (wd, _conn, id, state) = setup("search-fts");
        fs::create_dir_all(root(&wd).join("notes")).unwrap();
        fs::write(
            root(&wd).join("notes/записи.md"),
            "Клиент example.ru оплатил счёт за редизайн",
        )
        .unwrap();
        index::index_project(&state, id).unwrap(); // default config: noop embedder

        let hits = search_project(&state, id, "оплатил", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.rel_path, "notes/записи.md");
        assert_eq!(hit.name, "записи.md");
        assert_eq!(hit.kind, "file");
        assert_eq!(hit.match_types, ["fts"]);
        assert!(hit.snippet.contains("<<оплатил>>"), "{:?}", hit.snippet);
        assert!(hit.modified > 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fts_injection_attempts_are_treated_as_plain_tokens() {
        let (wd, _conn, id, state) = setup("search-inject");
        fs::write(root(&wd).join("near.md"), "near or far").unwrap();
        fs::write(root(&wd).join("only.md"), "near here").unwrap();
        fs::write(root(&wd).join("foo.md"), "foo bar").unwrap();
        index::index_project(&state, id).unwrap();

        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced", "NEAR", "foo*"] {
            assert!(
                search_project(&state, id, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        let query = |q: &str| rels(&search_project(&state, id, q, 10).unwrap());
        // operators become plain words: AND of both tokens, not an OR operator
        assert_eq!(query("NEAR OR"), ["near.md"]);
        assert_eq!(query("near OR"), ["near.md"]);
        assert_eq!(query("near OR foo"), Vec::<String>::new());
        // pure syntax yields no tokens, hence no results
        assert!(query("*").is_empty());
        assert!(query("\"").is_empty());
        assert!(query("\"unbalanced").is_empty());
        // "*" is stripped, the plain token still matches
        assert_eq!(query("foo"), ["foo.md"]);
        assert_eq!(query("foo*"), ["foo.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn hybrid_adds_semantic_only_hit_and_merges_match_types() {
        let (wd, conn, id, _state) = setup("search-hybrid");
        fs::write(root(&wd).join("кошки.md"), "кошки кошки кошки").unwrap();
        fs::write(root(&wd).join("самолет.md"), "самолет летит над морем").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        // differently worded query: FTS finds nothing, semantic finds the file
        assert!(fts_search(&conn, id, &fts_match_query(&tokenize("кошечка")), 10)
            .unwrap()
            .is_empty());
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошечка", 10).unwrap();
        assert_eq!(hits[0].rel_path, "кошки.md");
        assert_eq!(hits[0].match_types, ["semantic"]);
        assert!(!hits[0].snippet.is_empty());

        // literal word: both legs hit the same file, both matchTypes kept
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        assert_eq!(hits[0].rel_path, "кошки.md");
        assert_eq!(hits[0].match_types, ["fts", "semantic"]);

        // RRF ordering is deterministic across runs
        let again = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        assert_eq!(rels(&hits), rels(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn search_with_runs_every_leg_on_the_passed_connection() {
        // S1.2: one connection per call. File FTS, file semantic, task FTS,
        // task semantic, mtime and snippet legs must all work through the
        // passed conn without opening another one.
        let (wd, conn, id, _state) = setup("search-one-conn");
        fs::write(root(&wd).join("кошки.md"), "кошки кошки кошки").unwrap();
        tasks::task_create(&conn, id, "кошки в задаче", None).unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        crate::db::open_counter::arm();
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошки", 10).unwrap();
        let opens = crate::db::open_counter::stop();
        assert_eq!(opens, 0, "search legs must not open connections");

        let file = hits.iter().find(|h| h.kind == "file").expect("file hit");
        assert_eq!(file.match_types, ["fts", "semantic"]);
        assert!(file.modified > 0, "mtime leg must use the passed conn");
        let task = hits.iter().find(|h| h.kind == "task").expect("task hit");
        assert_eq!(task.match_types, ["fts", "semantic"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn enrichment_carries_per_hit_mtime_and_fallback_snippet() {
        // S1.3: the batched enrichment must yield exactly the values the old
        // per-hit lookups did: own mtime row per hit, FTS snippet kept,
        // semantic-only hits get a fallback snippet from stored content.
        let (wd, conn, id, _state) = setup("search-enrich-values");
        fs::write(root(&wd).join("а.md"), "слово в файле а").unwrap();
        fs::write(root(&wd).join("б.md"), "слово в файле б").unwrap();
        fs::write(root(&wd).join("в.md"), "кошки кошки кошки").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();
        for (rel, m) in [("а.md", 111), ("б.md", 222), ("в.md", 333)] {
            conn.execute(
                "UPDATE indexed_files SET mtime = ?1 WHERE rel_path = ?2",
                params![m, rel],
            )
            .unwrap();
        }

        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "слово", 10).unwrap();
        let by_rel = |rel: &str| hits.iter().find(|h| h.rel_path == rel).expect(rel);
        for (rel, m) in [("а.md", 111), ("б.md", 222)] {
            let hit = by_rel(rel);
            assert_eq!(hit.modified, m, "{rel} must carry its own mtime");
            assert!(hit.snippet.contains("<<слово>>"), "{rel}: {:?}", hit.snippet);
        }
        let sem = by_rel("в.md");
        assert_eq!(sem.match_types, ["semantic"]);
        assert_eq!(sem.modified, 333);
        assert!(sem.snippet.contains("кошки"), "{:?}", sem.snippet);
        // A file with no indexed row keeps the old fallbacks: mtime 0.
        conn.execute("DELETE FROM indexed_files WHERE rel_path = 'а.md'", [])
            .unwrap();
        let hits2 = search_with(&conn, &FakeEmbedder, 0.3, id, "слово", 10).unwrap();
        let stale = hits2.iter().find(|h| h.rel_path == "а.md").expect("а.md");
        assert_eq!(stale.modified, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn enrichment_uses_batched_queries_instead_of_one_per_hit() {
        // S1.3: statement trace must show exactly one mtime and one content
        // batch query for the whole call, no per-hit single-row lookups.
        let (wd, conn, id, _state) = setup("search-enrich-count");
        for name in ["а.md", "б.md", "в.md", "г.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        fs::write(root(&wd).join("sem.md"), "кошки кошки кошки").unwrap();
        index::index_root(
            &conn,
            id,
            &root(&wd),
            &IndexingConfig::default(),
            &FakeEmbedder,
        )
        .unwrap();

        thread_local! {
            static BATCHED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        fn count_batched(event: rusqlite::trace::TraceEvent<'_>) {
            let sql = match event {
                rusqlite::trace::TraceEvent::Stmt(_, sql) => sql,
                _ => return,
            };
            if sql.contains("rel_path IN (") {
                BATCHED.with(|c| c.set(c.get() + 1));
            }
        }
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_batched),
        );
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "слово", 10).unwrap();
        conn.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);

        assert!(hits.len() >= 5, "expected several file hits: {hits:?}");
        assert_eq!(
            BATCHED.with(|c| c.get()),
            2,
            "one mtime + one content batch query for {} hits",
            hits.len(),
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    // Representative cross-project query on the demo working dir
    // (~/Documents/PROJECT-S, PROJECT_S_WD override; indexed demo data and a
    // provisioned model). "Before" = crm_search's old shape: search_project
    // per project, each opening its own connection; "after" = one connection
    // and one embedder select for the whole call:
    //   cargo test --release search_conn_timing -- --ignored -- --nocapture
    #[test]
    #[ignore]
    fn search_conn_timing_one_connection_per_call() {
        use std::time::Instant;
        let wd = crate::embed::model_store::real_working_dir();
        let db_file = paths::db_path(&wd);
        assert!(db_file.exists(), "demo DB not found at {}", db_file.display());
        let state = AppState::new();
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        let pids: Vec<i64> = {
            let conn = db::open(&db_file).unwrap();
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(!pids.is_empty(), "demo DB has no projects");
        let query = "редизайн";
        let limit = 10usize;
        // Warm the embedder cache so ONNX session load is not timed.
        search_project(&state, pids[0], query, 1).unwrap();

        let mut runs = Vec::new();
        for round in 1..=3 {
            let started = Instant::now();
            let old_hits: usize = pids
                .iter()
                .map(|pid| search_project(&state, *pid, query, limit).unwrap().len())
                .sum();
            let old_ms = started.elapsed().as_secs_f64() * 1000.0;

            let started = Instant::now();
            let conn = db::open(&db_file).unwrap();
            let embedder = embed::select_embedder(&wd);
            let new_hits: usize = pids
                .iter()
                .map(|pid| {
                    search_with(&conn, embedder.as_ref(), embed::NAME_WEIGHT, *pid, query, limit)
                        .unwrap()
                        .len()
                })
                .sum();
            let new_ms = started.elapsed().as_secs_f64() * 1000.0;

            // The slice's actual saving: N opens (one per project) vs one open
            // for the whole call. The hybrid total is embedder-dominated, so
            // the FTS-only path (noop embedder: offline default and the
            // semantic-skipped case) is timed separately — there the per-call
            // open dominates.
            let started = Instant::now();
            let fts_old_hits: usize = pids
                .iter()
                .map(|pid| {
                    let conn = db::open(&db_file).unwrap();
                    search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, *pid, query, limit)
                        .unwrap()
                        .len()
                })
                .sum();
            let fts_old_ms = started.elapsed().as_secs_f64() * 1000.0;
            let started = Instant::now();
            let conn = db::open(&db_file).unwrap();
            let fts_new_hits: usize = pids
                .iter()
                .map(|pid| {
                    search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, *pid, query, limit)
                        .unwrap()
                        .len()
                })
                .sum();
            let fts_new_ms = started.elapsed().as_secs_f64() * 1000.0;

            assert_eq!(old_hits, new_hits, "round {round}: hits changed");
            assert_eq!(fts_old_hits, fts_new_hits, "round {round}: FTS hits changed");
            eprintln!(
                "search conn timing #{round}/3: hybrid cross-project call \
                 per-project-open={old_ms:.1} ms, one-open={new_ms:.1} ms \
                 ({new_hits} hits, {} projects); FTS-only per-project-open={fts_old_ms:.2} ms, \
                 one-open={fts_new_ms:.2} ms ({fts_new_hits} hits)",
                pids.len(),
            );
            runs.push((fts_old_ms, fts_new_ms));
        }
        assert!(runs.iter().all(|(many, one)| many > one));
    }

    // Representative FTS-only query on the demo working dir (S1.3). Two
    // measurements, 3 rounds each: the whole cross-project call (end-to-end,
    // noise-dominated at demo scale) and the enrichment stage in isolation —
    // the old per-hit point queries vs the batched enrich_hits. Enrichment is
    // idempotent, so both shapes run over the same already-enriched hits and
    // measure the same query work. The digest (rel_path|modified|snippet over
    // all hits, FNV-1a) must match between the two shapes:
    //   cargo test --release search_batch_enrich_timing -- --ignored --nocapture
    #[test]
    #[ignore]
    fn search_batch_enrich_timing_fts_only() {
        use std::time::Instant;
        let wd = crate::embed::model_store::real_working_dir();
        let db_file = paths::db_path(&wd);
        assert!(db_file.exists(), "demo DB not found at {}", db_file.display());
        let conn = db::open(&db_file).unwrap();
        let pids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(!pids.is_empty(), "demo DB has no projects");
        // "example" matches 29 of the 85 demo files: near-limit hit counts per
        // project, the worst realistic N+1 case on this DB.
        let query = "example";
        let limit = 10usize;
        let lowered: Vec<String> = tokenize(query).iter().map(|t| t.to_lowercase()).collect();
        search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, pids[0], query, limit).unwrap();

        // The per-hit shape this slice removes, kept here only to time it.
        fn old_file_mtime(conn: &Connection, project_id: i64, rel: &str) -> i64 {
            conn.query_row(
                "SELECT mtime FROM indexed_files WHERE project_id = ?1 AND rel_path = ?2",
                rusqlite::params![project_id, rel],
                |r| r.get(0),
            )
            .unwrap_or(0)
        }
        fn old_file_content(conn: &Connection, project_id: i64, rel: &str) -> Option<String> {
            conn.query_row(
                "SELECT content FROM files_fts WHERE project_id = ?1 AND rel_path = ?2",
                rusqlite::params![project_id.to_string(), rel],
                |r| r.get(0),
            )
            .ok()
        }
        let digest = |sets: &[(i64, Vec<SearchHit>)]| -> u64 {
            let mut hash: u64 = 0xcbf29ce484222325;
            for (_, hits) in sets {
                for h in hits {
                    for b in format!("{}|{}|{}\u{1}", h.rel_path, h.modified, h.snippet).bytes() {
                        hash ^= b as u64;
                        hash = hash.wrapping_mul(0x100000001b3);
                    }
                }
            }
            hash
        };

        let mut total_ms = 0.0;
        let mut call_digests = Vec::new();
        for round in 1..=3 {
            let started = Instant::now();
            let per_pid: Vec<(i64, Vec<SearchHit>)> = pids
                .iter()
                .map(|pid| {
                    (
                        *pid,
                        search_with(&conn, &embed::NoopEmbedder, embed::NAME_WEIGHT, *pid, query, limit)
                            .unwrap(),
                    )
                })
                .collect();
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            total_ms += ms;
            call_digests.push(digest(&per_pid));
            let hits: usize = per_pid.iter().map(|(_, h)| h.len()).sum();
            eprintln!(
                "search enrich timing #{round}/3: FTS-only cross-project call = {ms:.2} ms \
                 ({hits} hits, {} projects), digest = {:016x}",
                pids.len(),
                call_digests[round - 1],
            );

            let started = Instant::now();
            let mut old_set = per_pid.clone();
            for (pid, hits) in &mut old_set {
                for hit in hits.iter_mut() {
                    hit.modified = old_file_mtime(&conn, *pid, &hit.rel_path);
                    if hit.snippet.is_empty() {
                        let content = old_file_content(&conn, *pid, &hit.rel_path).unwrap_or_default();
                        hit.snippet = fallback_snippet(&content, &lowered);
                    }
                }
            }
            let old_ms = started.elapsed().as_secs_f64() * 1000.0;

            let started = Instant::now();
            let mut new_set = per_pid.clone();
            for (pid, hits) in &mut new_set {
                enrich_hits(&conn, *pid, hits, &lowered).unwrap();
            }
            let new_ms = started.elapsed().as_secs_f64() * 1000.0;

            assert_eq!(
                digest(&old_set),
                digest(&new_set),
                "round {round}: enrichment values changed"
            );
            eprintln!(
                "search enrich stage #{round}/3: per-hit queries = {old_ms:.3} ms, \
                 batched = {new_ms:.3} ms ({hits} hits)"
            );
            assert!(new_ms < old_ms, "round {round}: batching must be faster");
        }
        assert!(call_digests.windows(2).all(|w| w[0] == w[1]), "digest unstable");
        eprintln!(
            "search enrich timing: call mean = {:.2} ms, digest = {:016x}",
            total_ms / 3.0,
            call_digests[0],
        );
    }

    #[test]
    fn empty_query_returns_empty_and_noop_backend_stays_fts_only() {
        let (wd, _conn, id, state) = setup("search-empty");
        fs::write(root(&wd).join("a.txt"), "кошки").unwrap();
        index::index_project(&state, id).unwrap();

        assert!(search_project(&state, id, "", 10).unwrap().is_empty());
        assert!(search_project(&state, id, "   ", 10).unwrap().is_empty());
        let hits = search_project(&state, id, "кошки", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits.iter().all(|h| h.match_types == ["fts"]));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn limit_is_respected_and_results_are_scoped_to_project() {
        let (wd, conn, id, state) = setup("search-scope");
        let other = projects::create_project(&conn, &wd, "other").unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        fs::write(
            paths::projects_dir(&wd).join("other/x.md"),
            "слово из другого проекта",
        )
        .unwrap();
        index::index_project(&state, id).unwrap();
        index::index_project(&state, other.id).unwrap();

        assert_eq!(search_project(&state, id, "слово", 2).unwrap().len(), 2);
        let mut all = rels(&search_project(&state, id, "слово", 100).unwrap());
        all.sort();
        assert_eq!(all, ["a.md", "b.md", "c.md"]);

        let other_hits = search_project(&state, other.id, "слово", 10).unwrap();
        assert_eq!(rels(&other_hits), ["x.md"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_leg_does_not_leak_across_projects() {
        let (wd, conn, a, _state) = setup("search-sem-scope");
        let b = projects::create_project(&conn, &wd, "other").unwrap();
        fs::write(root(&wd).join("самолет.md"), "самолет летит над морем").unwrap();
        fs::write(
            paths::projects_dir(&wd).join("other/кошки.md"),
            "кошки кошки кошки",
        )
        .unwrap();
        let cfg = crate::config::IndexingConfig::default();
        index::index_root(&conn, a, &root(&wd), &cfg, &FakeEmbedder).unwrap();
        index::index_root(
            &conn,
            b.id,
            &paths::projects_dir(&wd).join("other"),
            &cfg,
            &FakeEmbedder,
        )
        .unwrap();

        // Query semantically closest to B's file: A's hits must carry only A's
        // files, B's rel_path must never surface in A's results.
        let hits_a = search_with(&conn, &FakeEmbedder, 0.3, a, "кошечка", 10).unwrap();
        assert!(
            !hits_a.iter().any(|h| h.rel_path == "кошки.md"),
            "B file leaked into A: {hits_a:?}"
        );
        assert!(hits_a.iter().all(|h| h.rel_path == "самолет.md" || h.rel_path == "datas.md"));

        let hits_b = search_with(&conn, &FakeEmbedder, 0.3, b.id, "кошечка", 10).unwrap();
        assert!(
            hits_b.iter().any(|h| h.rel_path == "кошки.md"),
            "B file missing from B: {hits_b:?}"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn query_matching_only_other_project_returns_empty_for_unindexed_project() {
        // FTS-only leg (noop embedder via search_project through AppState).
        let (wd, conn, a, state) = setup("search-scope-fts-empty");
        let b = projects::create_project(&conn, &wd, "other").unwrap();
        fs::write(
            paths::projects_dir(&wd).join("other/кошки.md"),
            "кошки кошки кошки",
        )
        .unwrap();
        index::index_project(&state, b.id).unwrap(); // A stays unindexed

        assert!(search_project(&state, a, "кошки", 10).unwrap().is_empty());
        assert_eq!(rels(&search_project(&state, b.id, "кошки", 10).unwrap()), ["кошки.md"]);

        // Semantic leg with a fake embedder over the same state.
        let fake = FakeEmbedder;
        let cfg = crate::config::IndexingConfig::default();
        index::index_root(&conn, b.id, &paths::projects_dir(&wd).join("other"), &cfg, &fake).unwrap();
        let hits_a = search_with(&conn, &fake, 0.3, a, "кошечка", 10).unwrap();
        assert!(hits_a.is_empty(), "B vectors leaked into A: {hits_a:?}");
        let hits_b = search_with(&conn, &fake, 0.3, b.id, "кошечка", 10).unwrap();
        assert!(
            hits_b.iter().any(|h| h.rel_path == "кошки.md"),
            "B file missing from B: {hits_b:?}"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_fts_hit_includes_closed_task_and_carries_task_fields() {
        let (wd, conn, id, state) = setup("search-task-closed");
        let t = tasks::task_create(&conn, id, "Сделать редизайн главной", None).unwrap();
        tasks::task_move(&conn, t.id, "done", 1.0).unwrap();

        let hits = search_project(&state, id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.kind, "task");
        assert_eq!(hit.rel_path, "");
        assert_eq!(hit.name, "Сделать редизайн главной");
        assert_eq!(hit.task_id, Some(t.id));
        assert_eq!(hit.status.as_deref(), Some("done"));
        assert_eq!(hit.overdue, Some(false));
        assert_eq!(hit.match_types, ["fts"]);
        assert!(hit.snippet.contains("редизайн"), "{:?}", hit.snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_title_and_comment_are_searchable_tags_are_not() {
        let (wd, conn, id, state) = setup("search-task-fields");
        let t = tasks::task_create(&conn, id, "Задача про баннер", None).unwrap();
        tasks::task_update(
            &conn,
            t.id,
            tasks::TaskPatch {
                comment: Some(Some("нужен макет логотипа".into())),
                ..Default::default()
            },
        )
        .unwrap();
        // Tags are out of the model: a legacy tags value on the row (written
        // straight to SQL) must not make the task findable.
        conn.execute("UPDATE tasks SET tags = '[\"полиграфия\"]' WHERE id = ?1", params![t.id])
            .unwrap();

        for query in ["баннер", "логотипа"] {
            let hits = search_project(&state, id, query, 10).unwrap();
            assert_eq!(hits.len(), 1, "query {query:?}");
            assert_eq!(hits[0].kind, "task");
        }
        assert!(search_project(&state, id, "полиграфия", 10).unwrap().is_empty(),
                "tag text must no longer be searchable");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_mutations_keep_search_in_sync() {
        let (wd, conn, id, state) = setup("search-task-sync");
        let find = |q: &str| {
            search_project(&state, id, q, 10)
                .unwrap()
                .iter()
                .any(|h| h.kind == "task")
        };
        let t = tasks::task_create(&conn, id, "старое слово", None).unwrap();
        assert!(find("старое"), "created task must be findable");
        tasks::task_update(
            &conn,
            t.id,
            tasks::TaskPatch {
                title: Some("новое слово".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(find("новое"), "updated text must be findable");
        assert!(!find("старое"), "old text must be gone after update");
        tasks::task_delete(&conn, t.id).unwrap();
        assert!(!find("новое"), "deleted task must be gone");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn mixed_query_returns_file_and_task_hits_with_deterministic_ranking() {
        let (wd, conn, id, state) = setup("search-task-mixed");
        fs::write(root(&wd).join("redizayn.md"), "редизайн каталога товаров").unwrap();
        index::index_project(&state, id).unwrap();
        tasks::task_create(&conn, id, "редизайн главной страницы", None).unwrap();

        let hits = search_project(&state, id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.kind == "file"));
        assert!(hits.iter().any(|h| h.kind == "task"));
        let again = search_project(&state, id, "редизайн", 10).unwrap();
        let keys = |hs: &[SearchHit]| -> Vec<(String, Option<i64>)> {
            hs.iter().map(|h| (h.kind.clone(), h.task_id)).collect()
        };
        assert_eq!(keys(&hits), keys(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_hits_are_scoped_to_project() {
        let (wd, conn, id, state) = setup("search-task-scope");
        let other = projects::create_project(&conn, &wd, "other").unwrap();
        tasks::task_create(&conn, other.id, "редизайн чужого проекта", None).unwrap();

        assert!(search_project(&state, id, "редизайн", 10).unwrap().is_empty());
        let hits = search_project(&state, other.id, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "task");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn semantic_leg_adds_task_hit_without_literal_match() {
        let (wd, conn, id, _state) = setup("search-task-sem");
        tasks::task_create(&conn, id, "кошки", None).unwrap();

        assert!(fts_search(&conn, id, &fts_match_query(&tokenize("кошечка")), 10)
            .unwrap()
            .is_empty());
        let hits = search_with(&conn, &FakeEmbedder, 0.3, id, "кошечка", 10).unwrap();
        let task = hits
            .iter()
            .find(|h| h.kind == "task")
            .expect("task semantic hit");
        assert_eq!(task.match_types, ["semantic"]);
        assert!(!task.snippet.is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    use crate::chat;
    use crate::harness;
    use crate::llm::Role;

    fn harness_session(conn: &Connection, title: &str) -> (i64, i64) {
        let s = harness::create_session(conn, Some(title)).unwrap();
        (s.id, harness::session_chat_id(conn, s.id).unwrap())
    }

    fn say(conn: &Connection, chat_id: i64, role: Role, text: &str) {
        chat::append_message(conn, chat_id, role, Some(text), &[], None, None).unwrap();
    }

    #[test]
    fn chat_search_finds_harness_session_by_cyrillic_fts() {
        let (wd, conn, _id, _state) = setup("chat-search-fts");
        let (sid, chat_id) = harness_session(&conn, "редизайн витрины");
        say(&conn, chat_id, Role::User, "нужен редизайн главной страницы");
        say(&conn, chat_id, Role::Assistant, "собрал макет, ждём правки");

        let hits = search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.session_id, sid);
        assert_eq!(hit.title.as_deref(), Some("редизайн витрины"));
        assert!(!hit.last_active.is_empty());
        assert!(hit.snippet.contains("<<редизайн>>"), "{:?}", hit.snippet);
        assert!(hit.score > 0.0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_groups_by_session_and_ranks_by_sum() {
        let (wd, conn, _id, _state) = setup("chat-search-group");
        let (a, chat_a) = harness_session(&conn, "a");
        let (b, chat_b) = harness_session(&conn, "b");
        say(&conn, chat_a, Role::User, "редизайн каталога");
        say(&conn, chat_a, Role::Assistant, "редизайн корзины готов");
        say(&conn, chat_b, Role::User, "редизайн ленты");

        let hits = search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, a, "two matches must outrank one");
        assert_eq!(hits[1].session_id, b);
        assert!(hits[0].score > hits[1].score);
        // Representative message belongs to session a.
        let a_msgs: Vec<i64> = conn
            .prepare("SELECT m.id FROM messages m JOIN chats c ON c.id = m.chat_id WHERE c.session_id = ?1")
            .unwrap()
            .query_map(params![a], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(a_msgs.contains(&hits[0].message_id));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_excludes_project_and_notes_chats() {
        let (wd, conn, pid, _state) = setup("chat-search-scope");
        let project_chat = chat::ensure_project_chat(&conn, pid).unwrap();
        say(&conn, project_chat, Role::User, "редизайн проекта");
        conn.execute(
            "INSERT INTO chats (kind, session_id, created_at, updated_at) VALUES ('notes', NULL, '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let notes_chat = conn.last_insert_rowid();
        say(&conn, notes_chat, Role::User, "редизайн заметок");

        assert!(search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_semantic_only_hit_without_literal_match() {
        let (wd, conn, _id, _state) = setup("chat-search-sem");
        let (sid, chat_a) = harness_session(&conn, "a");
        let (_, chat_b) = harness_session(&conn, "b");
        say(&conn, chat_a, Role::User, "кошки кошки кошки");
        say(&conn, chat_b, Role::User, "самолет летит над морем");
        for (chat_id, text) in [(chat_a, "кошки кошки кошки"), (chat_b, "самолет летит над морем")] {
            let message_id: i64 = conn
                .query_row(
                    "SELECT id FROM messages WHERE chat_id = ?1 AND role = 'user'",
                    params![chat_id],
                    |r| r.get(0),
                )
                .unwrap();
            let session_id: i64 = conn
                .query_row("SELECT session_id FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))
                .unwrap();
            chat::store_message_vector(&conn, &FakeEmbedder, message_id, session_id, text).unwrap();
        }

        let hits = search_chats_with(&conn, &FakeEmbedder, "кошечка", 10).unwrap();
        // KNN has no distance threshold: both sessions surface, the cat one
        // must rank first and carry the fallback snippet.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, sid);
        assert!(hits[0].score > hits[1].score);
        assert!(hits[0].snippet.contains("кошки"), "{:?}", hits[0].snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_survives_injection_and_respects_limit() {
        let (wd, conn, _id, _state) = setup("chat-search-inject");
        for i in 0..3 {
            let (_, chat_id) = harness_session(&conn, "s");
            say(&conn, chat_id, Role::User, &format!("редизайн {i}"));
        }
        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced"] {
            assert!(
                search_chats_with(&conn, &embed::NoopEmbedder, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        assert_eq!(search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 2).unwrap().len(), 2);
        assert!(search_chats_with(&conn, &embed::NoopEmbedder, "   ", 10).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn chat_search_drops_index_rows_left_by_deleted_sessions() {
        let (wd, conn, _id, _state) = setup("chat-search-stale");
        let (sid, chat_id) = harness_session(&conn, "a");
        say(&conn, chat_id, Role::User, "редизайн главной");
        assert_eq!(
            search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10)
                .unwrap()
                .len(),
            1
        );
        // Stale index state: FTS/vector rows survive, the session row does not.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![sid]).unwrap();
        assert!(search_chats_with(&conn, &embed::NoopEmbedder, "редизайн", 10)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    use crate::notes;

    #[test]
    fn global_search_finds_all_kinds_tags_hits_and_includes_closed_tasks() {
        let (wd, conn, id, state) = setup("global-kinds");
        fs::write(root(&wd).join("redizayn.md"), "редизайн каталога товаров").unwrap();
        index::index_project(&state, id).unwrap();
        let t = tasks::task_create(&conn, id, "редизайн главной", None).unwrap();
        tasks::task_move(&conn, t.id, "done", 1.0).unwrap();
        notes::ensure(&wd).unwrap();
        fs::write(paths::notes_dir(&wd).join("red.md"), "редизайн заметки").unwrap();
        index::index_notes(&wd).unwrap();
        let (sid, chat_id) = harness_session(&conn, "витрина");
        say(&conn, chat_id, Role::User, "редизайн чата");

        let hits = global_search_with(&conn, &embed::NoopEmbedder, "редизайн", 30).unwrap();
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
        for kind in ["file", "task", "note", "chat"] {
            assert!(kinds.contains(&kind), "missing {kind} in {hits:?}");
        }
        let file = hits.iter().find(|h| h.kind == "file").unwrap();
        assert_eq!(file.project_id, Some(id));
        assert_eq!(file.rel_path.as_deref(), Some("redizayn.md"));
        assert!(file.snippet.contains("<<редизайн>>"), "{:?}", file.snippet);
        let task = hits.iter().find(|h| h.kind == "task").unwrap();
        assert_eq!(task.task_id, Some(t.id));
        assert_eq!(task.project_id, Some(id));
        assert!(task.subtitle.contains("Готово"), "{:?}", task.subtitle);
        let note = hits.iter().find(|h| h.kind == "note").unwrap();
        assert_eq!(note.rel_path.as_deref(), Some("red.md"));
        assert_eq!(note.project_id, None);
        let chat = hits.iter().find(|h| h.kind == "chat").unwrap();
        assert_eq!(chat.session_id, Some(sid));
        // Deterministic merge: the same order on a second run.
        let again = global_search_with(&conn, &embed::NoopEmbedder, "редизайн", 30).unwrap();
        let keys = |hs: &[GlobalHit]| -> Vec<(String, i64)> {
            hs.iter().map(|h| (h.kind.clone(), h.id)).collect()
        };
        assert_eq!(keys(&hits), keys(&again));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn global_search_matches_cyrillic_project_name_case_insensitively() {
        let (wd, conn, _id, _state) = setup("global-project");
        let p = projects::create_project(&conn, &wd, "Ромашка").unwrap();
        let hits = global_search_with(&conn, &embed::NoopEmbedder, "ромашка", 30).unwrap();
        let project = hits.iter().find(|h| h.kind == "project").expect("project hit");
        assert_eq!(project.id, p.id);
        assert_eq!(project.project_id, Some(p.id));
        assert_eq!(project.title, "Ромашка");
        assert!(global_search_with(&conn, &embed::NoopEmbedder, "лютики", 30)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn global_search_dedupes_chat_title_and_message_hits_per_session() {
        let (wd, conn, _id, _state) = setup("global-chat-dedupe");
        let (sid, chat_id) = harness_session(&conn, "редизайн витрины");
        say(&conn, chat_id, Role::User, "нужен редизайн главной");
        let hits = global_search_with(&conn, &embed::NoopEmbedder, "редизайн", 30).unwrap();
        let chats: Vec<&GlobalHit> = hits.iter().filter(|h| h.kind == "chat").collect();
        assert_eq!(chats.len(), 1, "title and message hits must merge: {hits:?}");
        assert_eq!(chats[0].session_id, Some(sid));
        assert_eq!(chats[0].title, "редизайн витрины");
        assert!(chats[0].snippet.contains("<<редизайн>>"), "{:?}", chats[0].snippet);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn global_search_sanitizes_input_and_respects_limit() {
        let (wd, conn, id, state) = setup("global-limit");
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(root(&wd).join(name), "слово в тексте").unwrap();
        }
        index::index_project(&state, id).unwrap();

        for evil in ["\"", "*", "NEAR/3", "a OR b", "\"unbalanced"] {
            assert!(
                global_search_with(&conn, &embed::NoopEmbedder, evil, 10).is_ok(),
                "errored on {evil:?}"
            );
        }
        assert_eq!(
            global_search_with(&conn, &embed::NoopEmbedder, "слово", 2)
                .unwrap()
                .len(),
            2
        );
        assert!(global_search_with(&conn, &embed::NoopEmbedder, "   ", 10)
            .unwrap()
            .is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }
}
