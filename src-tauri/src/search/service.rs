use crate::error::Result;
use crate::state::AppState;
use crate::{db, embed, index, paths};
use rusqlite::{params, Connection};
use std::cmp::Ordering;
use std::collections::HashMap;

use super::chat::{chat_digest, message_content, session_info};
use super::enrich::enrich_hits;
use super::fts::{chat_fts_search, fts_search, task_fts_search};
use super::ranking::rrf_score;
use super::text::{fallback_snippet, fts_match_query, tokenize};
use super::types::{ChatHit, GlobalHit, SearchHit, TaskRow};
use super::vector::{chat_semantic_hits, task_semantic_hits};

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
    search_project(state, index::NOTES_SCOPE_ID, query, limit)
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
        hit.score += rrf_score(rank);
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
        hit.score += rrf_score(rank);
        hit.match_types.push("semantic".into());
    }
    // Task legs merge into the same RRF ranking, keyed by task id.
    let mut merged_tasks: HashMap<i64, SearchHit> = HashMap::new();
    for (rank, row) in task_rows.into_iter().enumerate() {
        let hit = merged_tasks
            .entry(row.id)
            .or_insert_with(|| task_hit(&row, &lowered));
        hit.score += rrf_score(rank);
        hit.match_types.push("fts".into());
    }
    for (rank, row) in task_semantic.into_iter().enumerate() {
        let hit = merged_tasks
            .entry(row.id)
            .or_insert_with(|| task_hit(&row, &lowered));
        hit.score += rrf_score(rank);
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

// Harness chat search: FTS5 over messages_fts plus KNN over vec_messages,
// merged per message with RRF, then aggregated per session. Session score =
// SUM of its matched messages' RRF scores (a session with several matches
// outranks a single-match one); the hit carries the session's best
// (highest-RRF) message and its snippet. Index rows left by deleted sessions
// or messages drop out when the session row is looked up.
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
        msg.score += rrf_score(rank);
    }
    for (rank, (message_id, session_id)) in semantic.into_iter().enumerate() {
        let msg = merged.entry(message_id).or_insert(Msg {
            message_id,
            session_id,
            score: 0.0,
            snippet: String::new(),
        });
        msg.score += rrf_score(rank);
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
            digest: chat_digest(conn, session_id),
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

// Global cross-entity search for the ⌘K palette: one FTS leg per entity
// (files across all projects, tasks including closed, the notes vault,
// harness chat messages via search_chats) plus plain substring legs for
// project and session titles (SQLite LIKE is ASCII-only, so Cyrillic
// case-folding happens in Rust; those tables stay small at local scale).
// Legs merge by RRF-style rank scores with a deterministic tiebreak.
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
                score: rrf_score(rank),
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
            params![match_query, index::NOTES_SCOPE_ID, limit as i64],
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
                score: rrf_score(rank),
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
                score: rrf_score(rank),
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
            params![match_query, index::NOTES_SCOPE_ID.to_string(), limit as i64],
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
                score: rrf_score(rank),
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
                entry.0 += rrf_score(rank);
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

fn task_status_label(status: &str) -> &'static str {
    match status {
        "new" => "Новые",
        "progress" => "В работе",
        "done" => "Готово",
        _ => "Задача",
    }
}
