// Global memory (SPEC 6.3): one SQLite store shared by project chats and
// harness sessions. Records carry facts/preferences/lessons; the user's
// gender lives here as a fixed-source identity record, injected into every
// context block. Search is hybrid over the v20 schema: FTS5 keyword leg over
// memory_fts (rowid = memory.id, content + tags) plus vector KNN over
// vec_memory (tracked by memory_vectors), merged with RRF, then weighted by
// recency decay (90-day half-life) and a tag boost. Identity records never
// compete in ranking: they surface separately through the stable identity
// block. With the Noop embedder the vector leg is skipped and ranking stays
// keyword-only. Before a chat range is folded into a checkpoint summary,
// extract_facts distills durable user/project facts into this store
// (source = "fact").
use crate::agent::r#loop::ChatBackend;
use crate::sync_util::lock_recover;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::db;
use crate::embed;
use crate::error::{AppError, Result};
use crate::llm::{ChatMessage, ChatParams};
use crate::paths;
use crate::search;
use crate::state::AppState;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

// Reciprocal rank fusion: each leg contributes 1 / (RRF_K + rank), rank
// starts at 1 (same constant as the file/chat hybrid search in search.rs).
const RRF_K: f64 = 60.0;
// Recency decay: a record's weight halves every 90 days since updated_at.
const RECENCY_HALF_LIFE_DAYS: f64 = 90.0;
// A query token matching one of the record's tags doubles its score.
const TAG_BOOST: f64 = 2.0;
// Records attached to the chat's own project are surfaced first (SPEC 6.3
// "подсвечиваются в его чате").
const PROJECT_BOOST: f64 = 2.0;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryDto {
    pub id: i64,
    pub content: String,
    pub project_id: Option<i64>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

// One ranked search hit: the record id plus the ranked fields (the UI and
// the agent tools consume id/content/tags/project plus the fusion score;
// forget targets records by these ids).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryHit {
    pub id: i64,
    pub content: String,
    pub tags: Vec<String>,
    pub project_id: Option<i64>,
    pub score: f64,
}

// Identity is fixed: the agent is always "Project-S" and always addresses
// the user informally with «ты». Only the user's gender is a learned memory
// record (set by the set_user_gender tool), injected into every block.
pub const IDENTITY_GENDER_SOURCE: &str = "identity:gender";
pub const AGENT_NAME: &str = "Project-S";
const GENDER_VALUES: [&str; 3] = ["male", "female", "unknown"];

// Header of the stable identity block kept in every run's system prompt.
pub const MEMORY_BLOCK_PREFIX: &str = "[Memory]";
// Header of the query-relevant records section of the volatile [Context]
// block on the fresh user turn (chat::context::volatile_block).
pub const RECORDS_HEADER: &str = "Relevant memory records:";
// Source tag of the durable facts distilled from a chat range before it is
// folded into a checkpoint summary (chat::context::build_context).
pub const FACT_SOURCE: &str = "fact";

// The injected sections stay short: at most this many ranked records and
// this many characters of record lines (the identity lines are always kept).
const MAX_BLOCK_RECORDS: usize = 6;
const MAX_BLOCK_CHARS: usize = 1_500;

// Cap on the facts one extraction pass may store.
const MAX_FACTS: usize = 8;

const MEMORY_COLS: &str = "id, content, project_id, tags, source, created_at, updated_at";

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    tags.into_iter().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect()
}

fn dto_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryDto> {
    let tags_raw: String = row.get(3)?;
    Ok(MemoryDto {
        id: row.get(0)?,
        content: row.get(1)?,
        project_id: row.get(2)?,
        tags: serde_json::from_str(&tags_raw).unwrap_or_default(),
        source: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn get(conn: &Connection, id: i64) -> Result<Option<MemoryDto>> {
    Ok(conn.query_row(
        &format!("SELECT {MEMORY_COLS} FROM memory WHERE id = ?1"),
        params![id],
        dto_from_row,
    )
    .optional()?)
}

// The v20 FTS mirror: rowid = memory.id, explicit memory_id column and an
// indexed tags column, so tag words are findable by keyword search.
fn fts_upsert(conn: &Connection, id: i64, content: &str, tags_json: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO memory_fts (rowid, memory_id, content, tags) VALUES (?1, ?1, ?2, ?3)",
        params![id, content, tags_json],
    )?;
    Ok(())
}

// One vector per record under rowid = memory.id; memory_vectors tracks which
// records already carry one so the backfill skips them.
pub(crate) fn store_memory_vector(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    id: i64,
    content: &str,
) -> Result<()> {
    let dim = embedder.dim();
    let mut vector = embedder
        .embed_texts(&[embed::doc_prefix("", content)])?
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
    conn.execute("DELETE FROM vec_memory WHERE rowid = ?1", params![id])?;
    conn.execute(
        "INSERT INTO vec_memory (rowid, embedding) VALUES (?1, ?2)",
        params![id, blob],
    )?;
    conn.execute(
        "INSERT INTO memory_vectors (memory_id, updated_at)
         VALUES (?1, datetime('now','localtime'))
         ON CONFLICT(memory_id) DO UPDATE SET updated_at = excluded.updated_at",
        params![id],
    )?;
    Ok(())
}

fn drop_memory_vector(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM vec_memory WHERE rowid = ?1", params![id])?;
    conn.execute("DELETE FROM memory_vectors WHERE memory_id = ?1", params![id])?;
    Ok(())
}

// The DB lives at <wd>/Settings/.db/project-s.db (paths::db_path), so the
// working dir is three levels up — where the embedder model files are. A DB
// outside that layout yields a wrong wd, the model check fails and the
// vector leg simply stays off.
fn working_dir_of_db(db_path: &Path) -> Option<std::path::PathBuf> {
    db_path.parent()?.parent()?.parent().map(Path::to_path_buf)
}

// Best-effort semantic leg on writes: own thread, own connection (WAL-safe),
// every failure ignored — the FTS mirror is the guaranteed index. In-memory
// connections (tests) have no path, so nothing is spawned.
fn spawn_memory_vector(conn: &Connection, id: i64, content: &str) {
    let Some(db_path) = conn.path().map(std::path::PathBuf::from) else {
        return;
    };
    let content = content.to_owned();
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
        let _ = store_memory_vector(&thread_conn, embedder.as_ref(), id, &content);
    });
}

// The embedder for read paths, derived from the connection's DB file: a real
// working dir yields the configured backend, anything else (in-memory test
// connections, foreign DB layouts) yields the Noop keyword-only fallback.
fn embedder_for(conn: &Connection) -> Arc<dyn embed::Embedder> {
    match conn.path().map(|p| working_dir_of_db(Path::new(p))) {
        Some(Some(wd)) => embed::select_embedder(&wd),
        _ => Arc::new(embed::NoopEmbedder),
    }
}

pub fn add(
    conn: &Connection,
    content: &str,
    project_id: Option<i64>,
    tags: Option<Vec<String>>,
    source: Option<&str>,
) -> Result<MemoryDto> {
    let content = content.trim();
    if content.is_empty() {
        return Err(AppError::Invalid("memory content is empty".into()));
    }
    let tags_json = serde_json::to_string(&normalize_tags(tags.unwrap_or_default()))?;
    conn.execute(
        "INSERT INTO memory (content, project_id, tags, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now','localtime'), datetime('now','localtime'))",
        params![content, project_id, tags_json, source],
    )?;
    let id = conn.last_insert_rowid();
    fts_upsert(conn, id, content, &tags_json)?;
    spawn_memory_vector(conn, id, content);
    get(conn, id)?.ok_or_else(|| AppError::Db(format!("memory {id} vanished after insert")))
}

pub fn update(conn: &Connection, id: i64, content: &str, tags: Option<Vec<String>>) -> Result<MemoryDto> {
    let content = content.trim();
    if content.is_empty() {
        return Err(AppError::Invalid("memory content is empty".into()));
    }
    let updated = conn.execute(
        "UPDATE memory SET content = ?1, updated_at = datetime('now','localtime') WHERE id = ?2",
        params![content, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("memory {id} not found")));
    }
    if let Some(tags) = tags {
        let tags_json = serde_json::to_string(&normalize_tags(tags))?;
        conn.execute("UPDATE memory SET tags = ?1 WHERE id = ?2", params![tags_json, id])?;
    }
    let tags_json: String = conn.query_row("SELECT tags FROM memory WHERE id = ?1", params![id], |r| r.get(0))?;
    fts_upsert(conn, id, content, &tags_json)?;
    drop_memory_vector(conn, id)?;
    spawn_memory_vector(conn, id, content);
    get(conn, id)?.ok_or_else(|| AppError::Db(format!("memory {id} vanished after update")))
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let deleted = conn.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![id])?;
    drop_memory_vector(conn, id)?;
    if deleted != 1 {
        return Err(AppError::Invalid(format!("memory {id} not found")));
    }
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<MemoryDto>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MEMORY_COLS} FROM memory ORDER BY updated_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map([], dto_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Backfill the FTS mirror and the vectors for records whose index rows are
// missing (legacy rows written before v20, or vectors skipped while the
// embedding model was absent). Noop embedder backfills FTS only and yields
// 0 vectors, not an error.
pub fn index_memory(state: &AppState) -> Result<usize> {
    let wd = state.working_dir()?;
    let conn = db::open(&paths::db_path(&wd))?;
    let embedder = embed::select_embedder(&wd);
    index_memory_with(&conn, embedder.as_ref())
}

pub(crate) fn index_memory_with(conn: &Connection, embedder: &dyn embed::Embedder) -> Result<usize> {
    conn.execute(
        "INSERT INTO memory_fts (rowid, memory_id, content, tags)
         SELECT id, id, content, tags FROM memory
         WHERE id NOT IN (SELECT rowid FROM memory_fts)",
        [],
    )?;
    if embedder.backend_id() == "noop" {
        return Ok(0);
    }
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT m.id, m.content
             FROM memory m
             LEFT JOIN memory_vectors v ON v.memory_id = m.id
             WHERE v.memory_id IS NULL
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut indexed = 0usize;
    for (id, content) in rows {
        // One failing embed must not abort the whole backfill.
        if store_memory_vector(conn, embedder, id, &content).is_ok() {
            indexed += 1;
        }
    }
    Ok(indexed)
}

// Keyword search via the FTS mirror; tokens are quoted phrases so FTS
// operator syntax in user input never survives (same defense as file
// search). The embedder is picked from the connection's DB file, so the
// vector leg is on exactly when the embedding model is available.
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<MemoryHit>> {
    let embedder = embedder_for(conn);
    search_with(conn, embedder.as_ref(), query, limit, None)
}

// Hybrid ranked search: FTS leg + vector KNN (skipped for the Noop
// embedder) merged with RRF, then score = rrf * recency_decay * tag_boost
// (* project_boost when the record belongs to prefer_project). Identity
// records never rank here: they surface separately via identity_block.
pub(crate) fn search_with(
    conn: &Connection,
    embedder: &dyn embed::Embedder,
    query: &str,
    limit: usize,
    prefer_project: Option<i64>,
) -> Result<Vec<MemoryHit>> {
    let terms = search::tokenize(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    // Both legs fetch headroom beyond the final limit: the final ranking
    // re-orders candidates by decay and boosts.
    let candidates = (limit * 5).clamp(10, 250);
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();

    struct Candidate {
        content: String,
        tags: Vec<String>,
        project_id: Option<i64>,
        age_days: f64,
        score: f64,
    }
    let mut merged: HashMap<i64, Candidate> = HashMap::new();

    {
        let mut stmt = conn.prepare(
            "SELECT m.id, m.content, m.project_id, m.tags,
                    COALESCE(julianday('now','localtime') - julianday(m.updated_at), 0)
             FROM memory_fts f JOIN memory m ON m.id = f.rowid
             WHERE memory_fts MATCH ?1 AND COALESCE(m.source, '') NOT LIKE 'identity:%'
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![search::fts_match_query(&terms), candidates as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })?;
        for (rank, row) in rows.enumerate() {
            let (id, content, project_id, tags_raw, age_days) = row?;
            let cand = merged.entry(id).or_insert_with(|| Candidate {
                content,
                tags: serde_json::from_str(&tags_raw).unwrap_or_default(),
                project_id,
                age_days,
                score: 0.0,
            });
            cand.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }

    if embedder.backend_id() != "noop" {
        let mut query_vector = embedder
            .embed_texts(&[embed::query_prefix(query)])?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Invalid("embedder returned no vector".into()))?;
        if query_vector.len() < embedder.dim() {
            return Err(AppError::Invalid("embedder returned too few dims".into()));
        }
        embed::truncate_renorm(&mut query_vector, embedder.dim());
        let blob: Vec<u8> = query_vector.iter().flat_map(|x| x.to_le_bytes()).collect();
        // Join back to memory: vectors left by deleted records never surface.
        let mut stmt = conn.prepare(
            "SELECT m.id, m.content, m.project_id, m.tags,
                    COALESCE(julianday('now','localtime') - julianday(m.updated_at), 0)
             FROM (SELECT rowid, distance FROM vec_memory WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance) q
             JOIN memory m ON m.id = q.rowid
             WHERE COALESCE(m.source, '') NOT LIKE 'identity:%'",
        )?;
        let rows = stmt.query_map(params![blob, candidates as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })?;
        for (rank, row) in rows.enumerate() {
            let (id, content, project_id, tags_raw, age_days) = row?;
            let cand = merged.entry(id).or_insert_with(|| Candidate {
                content,
                tags: serde_json::from_str(&tags_raw).unwrap_or_default(),
                project_id,
                age_days,
                score: 0.0,
            });
            cand.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }

    let mut scored: Vec<(f64, i64, MemoryHit)> = merged
        .into_iter()
        .map(|(id, cand)| {
            let decay = 0.5f64.powf(cand.age_days.max(0.0) / RECENCY_HALF_LIFE_DAYS);
            let tag_hit = cand.tags.iter().map(|t| t.to_lowercase()).any(|t| {
                !t.is_empty() && lowered.iter().any(|tok| tok.contains(&t) || t.contains(tok.as_str()))
            });
            let project_hit = prefer_project.is_some() && cand.project_id == prefer_project;
            let score = cand.score
                * decay
                * if tag_hit { TAG_BOOST } else { 1.0 }
                * if project_hit { PROJECT_BOOST } else { 1.0 };
            (
                score,
                id,
                MemoryHit {
                    id,
                    content: cand.content,
                    tags: cand.tags,
                    project_id: cand.project_id,
                    score,
                },
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    scored.truncate(limit);
    Ok(scored.into_iter().map(|(_, _, hit)| hit).collect())
}

// The fixed model-facing identity lines carried by every memory block.
fn identity_lines() -> String {
    format!("Your name is {AGENT_NAME}.\nAddress the user informally with «ты».")
}

// The gender line is always injected: a known gender drives Russian
// agreement, an unknown one keeps the model on gender-neutral phrasing.
fn gender_line(gender: &str) -> String {
    let guidance = match gender {
        "male" => "use masculine Russian agreement",
        "female" => "use feminine Russian agreement",
        _ => "use gender-neutral phrasing until it is known",
    };
    format!("User gender: {gender} ({guidance}).")
}

// The gender learned so far ("unknown" until set_user_gender records one).
pub fn user_gender(conn: &Connection) -> Result<String> {
    let content: Option<String> = conn
        .query_row(
            "SELECT content FROM memory WHERE source = ?1",
            params![IDENTITY_GENDER_SOURCE],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let gender = content
        .as_deref()
        .and_then(|c| c.rsplit_once(": ").map(|(_, v)| v))
        .filter(|v| GENDER_VALUES.contains(v))
        .unwrap_or("unknown");
    Ok(gender.to_owned())
}

// Upsert the gender record; the content reads naturally in the memory UI.
pub fn set_user_gender(conn: &Connection, gender: &str) -> Result<()> {
    upsert_identity(
        conn,
        IDENTITY_GENDER_SOURCE,
        &format!("User gender: {gender}"),
        &["identity".into(), "gender".into()],
    )
}

fn upsert_identity(conn: &Connection, source: &str, content: &str, tags: &[String]) -> Result<()> {
    let existing = conn
        .query_row(
            "SELECT id FROM memory WHERE source = ?1",
            params![source],
            |r| r.get::<_, i64>(0),
        )
        .optional()?;
    match existing {
        Some(id) => {
            update(conn, id, content, Some(tags.to_vec()))?;
        }
        None => {
            add(conn, content, None, Some(tags.to_vec()), Some(source))?;
        }
    }
    Ok(())
}

// The stable "[Memory]" block for the system prompt: the fixed identity lines
// plus the always-injected gender line, plus the global style-of-communication
// instruction (SPEC 6.6) when the user's profile is non-neutral. Nothing here
// depends on the fresh user turn's query, so the system prompt stays byte-identical
// across turns for a fixed profile and the prompt-cache prefix survives; the style
// line only changes as the profile itself evolves. Query-relevant records live in
// the volatile [Context] block instead (relevant_records).
pub fn identity_block(conn: &Connection) -> Result<String> {
    let gender = user_gender(conn)?;
    let mut block = format!(
        "{MEMORY_BLOCK_PREFIX}\n{}\n{}",
        identity_lines(),
        gender_line(&gender)
    );
    let style = crate::style::instruction_block(conn)?;
    if !style.is_empty() {
        block.push('\n');
        block.push_str(&style);
    }
    Ok(block)
}

// The query-relevant records section for the volatile [Context] block: the
// top ranked records (hybrid search, bounded by count and chars), formatted
// as "- …" lines under RECORDS_HEADER; records attached to the current
// project are boosted and marked for that chat (SPEC 6.3 "подсвечиваются").
// Identity records never appear here: they ride the stable identity block.
// Empty string when nothing matches, so the section can be dropped from the
// block entirely.
pub fn relevant_records(conn: &Connection, project_id: Option<i64>, query: &str) -> Result<String> {
    let embedder = embedder_for(conn);
    // Headroom beyond the block cap so the project-boosted record survives
    // the truncation even when many records match.
    let records = search_with(conn, embedder.as_ref(), query, MAX_BLOCK_RECORDS * 2, project_id)?;

    let mut lines = String::new();
    let mut chars = 0usize;
    for record in records {
        if lines.lines().count() >= MAX_BLOCK_RECORDS {
            break;
        }
        let content = record.content.replace('\n', " ");
        let line = if record.project_id.is_some() && record.project_id == project_id {
            format!("- {content} [this project]\n")
        } else {
            format!("- {content}\n")
        };
        if chars + line.chars().count() > MAX_BLOCK_CHARS {
            break;
        }
        chars += line.chars().count();
        lines.push_str(&line);
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{RECORDS_HEADER}\n{}", lines.trim_end()))
}

// FactExtractor (SPEC 6.3): before a chat range disappears into a checkpoint
// summary, ask the model to distill durable user/project facts into short
// Russian strings. Parsing is tolerant (code fences and surrounding prose
// are stripped, non-strings dropped), duplicates collapse by normalized
// content, and the batch is capped. A backend error returns Err — the
// caller must treat it as best-effort and never break compaction.
pub fn extract_facts_capped(
    backend: &dyn ChatBackend,
    params: &ChatParams,
    messages: &[ChatMessage],
    max_chars: usize,
) -> Result<Vec<String>> {
    let transcript = crate::chat::context::render_transcript_capped(messages, max_chars);
    if transcript.trim().is_empty() {
        return Ok(Vec::new());
    }
    let prompt = vec![
        ChatMessage::system(
            "You extract durable facts from a fragment of an agent conversation that is about \
to be compacted. Reply with ONLY a JSON array of short Russian strings: stable facts about \
the user (preferences, personal details, decisions) and about the project (clients, terms, \
technical decisions). Skip transient chatter, questions and tool noise. No explanations.",
        ),
        ChatMessage::user(transcript),
    ];
    let outcome = backend.chat_stream(&prompt, None, params, &mut |_| {})?;
    Ok(parse_facts(&outcome.content.unwrap_or_default()))
}

fn parse_facts(raw: &str) -> Vec<String> {
    let stripped = raw.replace("```json", "").replace("```", "");
    let (Some(start), Some(end)) = (stripped.find('['), stripped.rfind(']')) else {
        return Vec::new();
    };
    let Ok(values) = serde_json::from_str::<Vec<Value>>(&stripped[start..=end]) else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut facts = Vec::new();
    for value in values {
        let Some(text) = value.as_str().map(str::trim) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let normalized: String = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        if !seen.insert(normalized) {
            continue;
        }
        facts.push(text.to_owned());
        if facts.len() >= MAX_FACTS {
            break;
        }
    }
    facts
}

// Borrows the shared app-state connection for a tool call (same pattern as
// the project tools).
fn db_guard(ctx: &ToolCtx) -> Result<std::sync::MutexGuard<'_, Option<Connection>>> {
    let state = ctx.state.as_deref().ok_or(AppError::NotConfigured)?;
    Ok(lock_recover(&state.db))
}

// remember (SPEC 5.2/6.3): writing user preferences into global memory is
// intentional, so the Mutating tool stays Skip.
pub struct RememberTool;

impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }
    fn description(&self) -> &str {
        "Save a durable fact, preference, decision or lesson to the global memory shared by all project chats and harness sessions. Set project_id only for facts about one specific project."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "project_id": { "type": "number" },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let content = args["content"].as_str().unwrap_or_default();
        if content.trim().is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "content must not be empty"));
        }
        let project_id = args.get("project_id").and_then(Value::as_i64);
        let tags = args
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect());
        let guard = db_guard(ctx)?;
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        if let Some(pid) = project_id {
            let exists: bool = conn
                .query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)", params![pid], |r| r.get(0))?;
            if !exists {
                return Ok(ToolResult::err(ToolErrorCode::NotFound, format!("project {pid} not found")));
            }
        }
        match add(conn, content, project_id, tags, Some("agent")) {
            Ok(dto) => Ok(ToolResult::ok(json!({ "id": dto.id, "content": dto.content }))),
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string())),
        }
    }
}

// recall: read-only hybrid ranked search over the same global store.
pub struct RecallTool;

impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        "Search the global memory (facts, preferences, decisions, lessons shared across all chats and sessions) by keywords; results are ranked by relevance."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "number" }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as usize;
        let guard = db_guard(ctx)?;
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        match search(conn, query, limit) {
            Ok(records) => Ok(ToolResult::ok(json!({ "memories": records })).capped(self.max_result_len())),
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string())),
        }
    }
}

// set_user_gender: the agent persists the gender it infers from the
// conversation (never asks the user); the record feeds the always-injected
// gender line so Russian agreement follows the user. Mutating stays Skip:
// writing user facts into global memory is intentional (like remember).
pub struct SetUserGenderTool;

impl Tool for SetUserGenderTool {
    fn name(&self) -> &str {
        "set_user_gender"
    }
    fn description(&self) -> &str {
        "Persist the user's gender for correct Russian agreement after inferring it from the conversation or the user's own words (never ask for it): male, female, or unknown (to reset)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "gender": { "type": "string", "enum": ["male", "female", "unknown"] }
            },
            "required": ["gender"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let gender = args["gender"].as_str().unwrap_or_default();
        if !GENDER_VALUES.contains(&gender) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "gender must be one of: male, female, unknown",
            ));
        }
        let guard = db_guard(ctx)?;
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        match set_user_gender(conn, gender) {
            Ok(()) => Ok(ToolResult::ok(json!({ "gender": gender }))),
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string())),
        }
    }
}

// Best-effort UI refresh after an agent memory mutation: the same event and
// payload shape the memory_* commands in lib.rs emit; a missing emitter is
// not an error.
fn emit_memory_changed(ctx: &ToolCtx, id: Option<i64>) {
    if let Some(state) = ctx.state.as_deref() {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("memory-changed", json!({ "id": id }));
        }
    }
}

// Candidate preview for the ambiguous-query error: one line, bounded width.
fn short_content(content: &str) -> String {
    let mut line: String = content
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .take(80)
        .collect();
    if content.chars().count() > 80 {
        line.push('…');
    }
    line
}

// How many candidates an ambiguous forget query reports back to the model.
const FORGET_QUERY_LIMIT: usize = 5;

// forget: the user asks the agent to drop or correct a memory record.
// Deletion is destructive, so unlike remember this Mutating tool is
// Check-confirmed. Targeting: an explicit id wins; otherwise the query must
// match exactly one record — several matches come back as an invalid_args
// candidate list (id + short content) so the agent can ask the user or pick
// an id, none comes back as not_found. With replacement the record is
// updated (FTS re-mirrored, stale vector dropped and re-embedded) instead of
// deleted.
pub struct ForgetTool;

impl Tool for ForgetTool {
    fn name(&self) -> &str {
        "forget"
    }
    fn description(&self) -> &str {
        // Two lines on purpose: the prompt tool-list block carries only the
        // first line, the full two-line text reaches the tool schema.
        "Forget or correct a global memory record when the user asks to remove or fix something remembered.
Target by id, or by query: a unique match is acted on, several matches return an invalid_args error listing candidate ids (ask the user or pick one), none returns not_found. Provide replacement to correct the content instead of deleting it. At least one of id or query is required."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "number" },
                "query": { "type": "string" },
                "replacement": { "type": "string" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        let target = match args.get("id").and_then(Value::as_i64) {
            Some(id) => format!("#{id}"),
            None => match args.get("query").and_then(Value::as_str) {
                Some(q) if !q.trim().is_empty() => format!("по запросу «{}»", q.trim()),
                _ => String::new(),
            },
        };
        Some(match args.get("replacement").and_then(Value::as_str) {
            Some(r) if !r.trim().is_empty() => format!("замена текста записи памяти {target}"),
            _ => format!("удаление записи памяти {target}"),
        })
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64);
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if id.is_none() && query.is_empty() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "provide at least one of id or query",
            ));
        }
        let replacement = args
            .get("replacement")
            .and_then(Value::as_str)
            .filter(|r| !r.trim().is_empty());
        let guard = db_guard(ctx)?;
        let conn = guard.as_ref().ok_or(AppError::NotConfigured)?;
        let target = match id {
            Some(id) => id,
            None => {
                let hits = match search(conn, &query, FORGET_QUERY_LIMIT) {
                    Ok(hits) => hits,
                    Err(e) => {
                        return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()))
                    }
                };
                match hits.len() {
                    0 => {
                        return Ok(ToolResult::err(
                            ToolErrorCode::NotFound,
                            format!("no memory record matches {query:?}"),
                        ))
                    }
                    1 => hits[0].id,
                    _ => {
                        let candidates = hits
                            .iter()
                            .map(|h| format!("{}: {:?}", h.id, short_content(&h.content)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Ok(ToolResult::err(
                            ToolErrorCode::InvalidArgs,
                            format!(
                                "several memory records match {query:?}; retry with one of these ids: {candidates}"
                            ),
                        ));
                    }
                }
            }
        };
        let attempt = match replacement {
            Some(replacement) => update(conn, target, replacement, None)
                .map(|dto| ("updated", dto.id, Some(dto.content))),
            None => get(conn, target)?.map_or_else(
                || Err(AppError::Invalid(format!("memory {target} not found"))),
                |dto| {
                    delete(conn, target).map(|()| ("deleted", target, Some(dto.content)))
                },
            ),
        };
        match attempt {
            Ok((action, id, content)) => {
                emit_memory_changed(ctx, if action == "updated" { Some(id) } else { None });
                Ok(ToolResult::ok(json!({ "action": action, "id": id, "content": content })))
            }
            Err(e) => {
                let msg = e.to_string();
                let code = if msg.contains("not found") {
                    ToolErrorCode::NotFound
                } else {
                    ToolErrorCode::ExecutionError
                };
                Ok(ToolResult::err(code, msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Resolution;
    use crate::db::schema;
    use crate::llm::{ChatEvent, ChatOutcome, ToolSpec};
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::state::UiEmitter;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

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

    // Deterministic stand-in: byte-bucket counts, no network (same recipe as
    // the search.rs hybrid tests).
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

    struct CannedBackend {
        reply: String,
    }

    impl ChatBackend for CannedBackend {
        fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[ToolSpec]>,
            _params: &ChatParams,
            _on_event: &mut dyn FnMut(ChatEvent),
        ) -> Result<ChatOutcome> {
            Ok(ChatOutcome {
                content: Some(self.reply.clone()),
                reasoning: None,
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: None,
            })
        }
    }

    fn test_params() -> ChatParams {
        ChatParams {
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            requires_reasoning_passback: false,
        }
    }

    fn fts_row(conn: &Connection, id: i64) -> Option<(i64, String, String)> {
        conn.query_row(
            "SELECT memory_id, content, tags FROM memory_fts WHERE rowid = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn crud_round_trip_keeps_tags_fts_and_vectors_in_sync() {
        let (wd, conn) = setup("mem-crud");
        let dto = add(&conn, "предпочитает короткие ответы", None, Some(vec!["pref".into(), " ".into()]), Some("user")).unwrap();
        assert_eq!(dto.tags, vec!["pref"]);
        assert_eq!(dto.source.as_deref(), Some("user"));
        assert_eq!(list(&conn).unwrap().len(), 1);
        let fts = fts_row(&conn, dto.id).expect("fts row after add");
        assert_eq!(fts.0, dto.id, "memory_id mirrors the rowid");
        assert_eq!(fts.1, "предпочитает короткие ответы");
        assert_eq!(fts.2, "[\"pref\"]", "tags column carries the record tags");

        let updated = update(&conn, dto.id, "предпочитает очень короткие ответы", Some(vec![])).unwrap();
        assert_eq!(updated.content, "предпочитает очень короткие ответы");
        assert!(updated.tags.is_empty());
        let fts = fts_row(&conn, dto.id).expect("fts row after update");
        assert_eq!(fts.1, "предпочитает очень короткие ответы");
        assert_eq!(fts.2, "[]", "fts tags follow the record tags");

        // vectors: store, then delete must clean both vector tables
        store_memory_vector(&conn, &FakeEmbedder, dto.id, &updated.content).unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM vec_memory WHERE rowid = ?1", params![dto.id], |r| r.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM memory_vectors WHERE memory_id = ?1", params![dto.id], |r| r.get(0))
                .unwrap(),
            1
        );

        assert!(matches!(update(&conn, 999, "x", None), Err(AppError::Invalid(_))));
        assert!(matches!(add(&conn, "   ", None, None, None), Err(AppError::Invalid(_))));

        delete(&conn, dto.id).unwrap();
        assert!(matches!(delete(&conn, dto.id), Err(AppError::Invalid(_))));
        assert!(list(&conn).unwrap().is_empty());
        assert!(fts_row(&conn, dto.id).is_none(), "fts row must be deleted with the record");
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM vec_memory WHERE rowid = ?1", params![dto.id], |r| r.get(0))
                .unwrap(),
            0,
            "vector row must be deleted with the record"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM memory_vectors WHERE memory_id = ?1", params![dto.id], |r| r.get(0))
                .unwrap(),
            0
        );
        assert!(search(&conn, "короткие", 10).unwrap().is_empty(), "fts row must be deleted with the record");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn search_finds_cyrillic_and_survives_operator_syntax() {
        let (wd, conn) = setup("mem-search");
        add(&conn, "Клиент example.ru — аптечная сеть", None, None, None).unwrap();
        add(&conn, "Погода в Москве", None, None, None).unwrap();

        let hits = search(&conn, "аптечная", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("аптечная"));
        assert!(hits[0].score > 0.0);
        assert!(search(&conn, "OR NEAR \"", 10).unwrap().is_empty());
        assert!(search(&conn, "", 10).unwrap().is_empty());
        // keyword search needs no embedder: the Noop default finds records too
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn tag_words_are_searchable_through_the_fts_tags_column() {
        let (wd, conn) = setup("mem-tag-search");
        add(&conn, "данные для доступа", None, Some(vec!["клиент".into()]), None).unwrap();
        add(&conn, "совсем другое", None, None, None).unwrap();

        let hits = search(&conn, "клиент", 10).unwrap();
        assert_eq!(hits.len(), 1, "the tag token must be findable");
        assert_eq!(hits[0].tags, vec!["клиент"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn ranking_favors_recent_and_tag_matching_records() {
        let (wd, conn) = setup("mem-rank");
        add(&conn, "кофе свежий", None, None, None).unwrap();
        let old = add(&conn, "кофе старый", None, None, None).unwrap();
        conn.execute(
            "UPDATE memory SET updated_at = datetime('now','localtime','-200 days') WHERE id = ?1",
            params![old.id],
        )
        .unwrap();

        let hits = search(&conn, "кофе", 10).unwrap();
        assert_eq!(hits[0].content, "кофе свежий", "recency decay must lift the fresh record");
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[1].content, "кофе старый");

        // tag boost: the tagged record matches the query token through its
        // tags column and outranks the untagged one at equal age.
        add(&conn, "кофе напитки без тегов", None, None, None).unwrap();
        add(&conn, "кофе", None, Some(vec!["напитки".into()]), None).unwrap();
        let hits = search(&conn, "кофе напитки", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].tags, vec!["напитки"], "tag match must double the RRF score");
        assert!(hits[0].score > hits[1].score);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn vector_leg_surfaces_semantic_only_record_and_noop_stays_keyword_only() {
        let (wd, conn) = setup("mem-vector");
        let cat = add(&conn, "кошки кошки кошки", None, None, None).unwrap();
        let plane = add(&conn, "самолет летит над морем", None, None, None).unwrap();
        store_memory_vector(&conn, &FakeEmbedder, cat.id, &cat.content).unwrap();
        store_memory_vector(&conn, &FakeEmbedder, plane.id, &plane.content).unwrap();

        // differently worded query: FTS finds nothing, the vector leg does
        let hits = search_with(&conn, &FakeEmbedder, "кошечка", 10, None).unwrap();
        assert_eq!(hits[0].content, "кошки кошки кошки");

        // Noop embedder: no vector leg at all, the same query finds nothing,
        // but the keyword leg still works.
        assert!(search_with(&conn, &embed::NoopEmbedder, "кошечка", 10, None).unwrap().is_empty());
        assert!(!search_with(&conn, &embed::NoopEmbedder, "кошки", 10, None).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn index_memory_backfills_missing_fts_rows_and_vectors() {
        let (wd, conn) = setup("mem-index");
        let a = add(&conn, "запись одна кофе", None, None, None).unwrap();
        let b = add(&conn, "запись две кофе", None, None, None).unwrap();
        // Simulate legacy rows whose FTS mirror entries are missing.
        conn.execute("DELETE FROM memory_fts WHERE rowid IN (?1, ?2)", params![a.id, b.id])
            .unwrap();
        assert!(search(&conn, "кофе", 10).unwrap().is_empty());

        // Noop: FTS backfilled, no vectors.
        assert_eq!(index_memory_with(&conn, &embed::NoopEmbedder).unwrap(), 0);
        assert_eq!(search(&conn, "кофе", 10).unwrap().len(), 2);

        // A real embedder backfills the missing vectors; the second pass is
        // a no-op because every record already carries one.
        assert_eq!(index_memory_with(&conn, &FakeEmbedder).unwrap(), 2);
        assert_eq!(index_memory_with(&conn, &FakeEmbedder).unwrap(), 0);
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM memory_vectors", [], |r| r.get(0))
                .unwrap(),
            2
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn extract_facts_parses_fenced_json_dedupes_and_caps() {
        let mut items: Vec<String> = (0..12).map(|i| format!("Факт {i}")).collect();
        items.push("факт 0".into()); // case-insensitive duplicate
        items.push("  Факт 1  ".into()); // whitespace duplicate
        items.push(String::new()); // empty entry skipped
        let reply = format!(
            "Вот факты:\n```json\n{}\n```\nГотово.",
            serde_json::to_string(&items).unwrap()
        );
        let backend = CannedBackend { reply };
        let cap = crate::chat::context::SUMMARIZER_MAX_CHARS;
        let facts =
            extract_facts_capped(&backend, &test_params(), &[ChatMessage::user("привет")], cap)
                .unwrap();
        assert_eq!(facts.len(), MAX_FACTS, "batch must be capped");
        assert_eq!(facts[0], "Факт 0");
        assert_eq!(facts[1], "Факт 1");
        assert!(facts.iter().all(|f| !f.trim().is_empty()));
        let mut seen: Vec<String> = facts.iter().map(|f| f.to_lowercase()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), facts.len(), "duplicates must collapse");
    }

    #[test]
    fn extract_facts_tolerates_garbage_and_strips_fences() {
        let fenced = CannedBackend {
            reply: "```json\n[\"Пользователь любит кофе\"]\n```".into(),
        };
        let cap = crate::chat::context::SUMMARIZER_MAX_CHARS;
        let facts =
            extract_facts_capped(&fenced, &test_params(), &[ChatMessage::user("кофе")], cap)
                .unwrap();
        assert_eq!(facts, vec!["Пользователь любит кофе"]);

        let prose = CannedBackend { reply: "суммаризатор выдал обычный текст".into() };
        assert!(extract_facts_capped(&prose, &test_params(), &[ChatMessage::user("x")], cap)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn project_attached_record_is_nulled_when_project_deleted() {
        let (wd, conn) = setup("mem-fk");
        let project = projects::create_project(&conn, &wd, "proj").unwrap();
        let dto = add(&conn, "факт про проект", Some(project.id), None, None).unwrap();
        assert_eq!(dto.project_id, Some(project.id));

        conn.execute("DELETE FROM projects WHERE id = ?1", params![project.id]).unwrap();
        let after = get(&conn, dto.id).unwrap().expect("record must survive project deletion");
        assert_eq!(after.project_id, None);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn remember_and_recall_registered_in_both_catalogs_and_work() {
        let (wd, conn) = setup("mem-tools");
        let project = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            scope: None,
            cancel: None,
            project_id: Some(project.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(Arc::clone(&state)),
            notes_root: None,
        };

        for (i, catalog) in [
            crate::agent::project_tools::project_catalog(),
            crate::harness::harness_catalog(),
        ]
        .into_iter()
        .enumerate()
        {
            let remember = match catalog.resolve("remember") {
                Resolution::Found(t) => t.clone(),
                _ => panic!("remember missing from catalog"),
            };
            let recall = match catalog.resolve("recall") {
                Resolution::Found(t) => t.clone(),
                _ => panic!("recall missing from catalog"),
            };
            assert_eq!(remember.class(), ToolClass::Mutating);
            assert_eq!(remember.policy(), PermissionPolicy::Skip);
            assert_eq!(recall.class(), ToolClass::ReadOnly);
            assert_eq!(recall.policy(), PermissionPolicy::Skip);

            // Distinct content per catalog: both catalogs share one DB here.
            let r = remember
                .execute(
                    &json!({ "content": format!("Клиент example.ru — аптечная сеть {i}"), "project_id": project.id, "tags": ["клиент"] }),
                    &ctx,
                )
                .unwrap();
            assert!(r.ok, "{:?}", r.error);

            let bad = remember.execute(&json!({ "content": "x", "project_id": 9999 }), &ctx).unwrap();
            assert_eq!(bad.error.unwrap().code, "not_found");

            let r = recall.execute(&json!({ "query": format!("аптечная {i}") }), &ctx).unwrap();
            assert!(r.ok, "{:?}", r.error);
            let memories = r.data.unwrap()["memories"].as_array().unwrap().clone();
            assert_eq!(memories.len(), 1);
            assert_eq!(memories[0]["projectId"], project.id);
            assert_eq!(memories[0]["tags"], json!(["клиент"]));
            assert!(memories[0]["score"].as_f64().unwrap() > 0.0);
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn set_user_gender_upserts_exactly_one_identity_record() {
        let (wd, conn) = setup("mem-gender");
        assert_eq!(user_gender(&conn).unwrap(), "unknown");
        set_user_gender(&conn, "female").unwrap();
        set_user_gender(&conn, "male").unwrap();
        let gender: Vec<MemoryDto> = list(&conn)
            .unwrap()
            .into_iter()
            .filter(|m| m.source.as_deref() == Some(IDENTITY_GENDER_SOURCE))
            .collect();
        assert_eq!(gender.len(), 1);
        assert_eq!(gender[0].content, "User gender: male");
        assert_eq!(gender[0].tags, vec!["identity", "gender"]);
        assert_eq!(user_gender(&conn).unwrap(), "male");
        set_user_gender(&conn, "unknown").unwrap();
        assert_eq!(user_gender(&conn).unwrap(), "unknown");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn set_user_gender_registered_in_both_catalogs_validates_and_updates() {
        let (wd, conn) = setup("mem-gender-tools");
        let state = Arc::new(AppState::new());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            scope: None,
            project_id: None,
            cancel: None,
            session_id: None,
            chat_id: None,
            working_dir: None,
            state: Some(Arc::clone(&state)),
            notes_root: None,
        };

        for catalog in [
            crate::agent::project_tools::project_catalog(),
            crate::harness::harness_catalog(),
        ] {
            let tool = match catalog.resolve("set_user_gender") {
                Resolution::Found(t) => t.clone(),
                _ => panic!("set_user_gender missing from catalog"),
            };
            assert_eq!(tool.class(), ToolClass::Mutating);
            assert_eq!(tool.policy(), PermissionPolicy::Skip);

            let bad = tool.execute(&json!({ "gender": "other" }), &ctx).unwrap();
            assert_eq!(bad.error.unwrap().code, "invalid_args");
            let bad = tool.execute(&json!({}), &ctx).unwrap();
            assert_eq!(bad.error.unwrap().code, "invalid_args");

            let r = tool.execute(&json!({ "gender": "female" }), &ctx).unwrap();
            assert!(r.ok, "{:?}", r.error);

            let guard = state.db.lock().unwrap();
            let records = list(guard.as_ref().unwrap()).unwrap();
            let gender: Vec<&MemoryDto> = records
                .iter()
                .filter(|m| m.source.as_deref() == Some(IDENTITY_GENDER_SOURCE))
                .collect();
            assert_eq!(gender.len(), 1);
            assert_eq!(gender[0].content, "User gender: female");
            assert_eq!(gender[0].tags, vec!["identity", "gender"]);
            drop(guard);
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[derive(Default)]
    struct RecordingEmitter {
        events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl UiEmitter for RecordingEmitter {
        fn emit_event(&self, event: &str, payload: Value) {
            self.events.lock().unwrap().push((event.to_owned(), payload));
        }
    }

    type RecordedEvents = Arc<Mutex<Vec<(String, Value)>>>;

    // AppState wired with a recording emitter and the test connection moved in.
    fn tool_state(conn: Connection) -> (Arc<AppState>, RecordedEvents) {
        let state = Arc::new(AppState::new());
        let recorder = RecordingEmitter::default();
        let events = Arc::clone(&recorder.events);
        *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
        *state.db.lock().unwrap() = Some(conn);
        (state, events)
    }

    fn forget_ctx(state: &Arc<AppState>) -> ToolCtx {
        ToolCtx {
            state: Some(Arc::clone(state)),
            ..Default::default()
        }
    }

    #[test]
    fn forget_by_id_deletes_record_and_cleans_fts_vectors() {
        let (wd, conn) = setup("mem-forget-del");
        let dto = add(&conn, "Клиент example.ru — аптечная сеть", None, Some(vec!["клиент".into()]), None).unwrap();
        store_memory_vector(&conn, &FakeEmbedder, dto.id, &dto.content).unwrap();
        let (state, events) = tool_state(conn);
        let ctx = forget_ctx(&state);

        let r = ForgetTool.execute(&json!({ "id": dto.id }), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["action"], "deleted");
        assert_eq!(data["id"], dto.id);
        assert!(data["content"].as_str().unwrap().contains("аптечная"));

        let guard = state.db.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        assert!(list(conn).unwrap().is_empty());
        assert!(fts_row(conn, dto.id).is_none(), "fts row must go with the record");
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM vec_memory WHERE rowid = ?1", params![dto.id], |r| r.get(0)).unwrap(),
            0,
            "vector row must go with the record"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM memory_vectors WHERE memory_id = ?1", params![dto.id], |r| r.get(0)).unwrap(),
            0
        );
        assert!(search(conn, "аптечная", 10).unwrap().is_empty());
        drop(guard);

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "memory-changed");
        assert!(events[0].1["id"].is_null(), "delete notifies with a null id like the UI command");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn forget_with_replacement_updates_content_and_reindexes() {
        let (wd, conn) = setup("mem-forget-upd");
        let dto = add(&conn, "Клиент example.ru — аптечная сеть", None, None, None).unwrap();
        store_memory_vector(&conn, &FakeEmbedder, dto.id, &dto.content).unwrap();
        let (state, events) = tool_state(conn);
        let ctx = forget_ctx(&state);

        let r = ForgetTool
            .execute(&json!({ "id": dto.id, "replacement": "Клиент example.ru — сеть клиник" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["action"], "updated");
        assert_eq!(data["id"], dto.id);
        assert_eq!(data["content"], "Клиент example.ru — сеть клиник");

        let guard = state.db.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        let fts = fts_row(conn, dto.id).expect("fts row after update");
        assert_eq!(fts.1, "Клиент example.ru — сеть клиник", "fts mirror must follow the new content");
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT count(*) FROM vec_memory WHERE rowid = ?1", params![dto.id], |r| r.get(0)).unwrap(),
            0,
            "the stale vector must be dropped on update (re-embed is spawned on real DBs)"
        );
        assert!(search(conn, "клиник", 10).unwrap().iter().any(|h| h.id == dto.id));
        drop(guard);

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "memory-changed");
        assert_eq!(events[0].1["id"], dto.id);

        let r = ForgetTool.execute(&json!({ "id": 999 }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        assert_eq!(events.len(), 1, "a failed forget must not notify");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn forget_by_query_unique_ambiguous_and_no_match() {
        let (wd, conn) = setup("mem-forget-query");
        let clinic = add(&conn, "Клиент example.ru — аптечная сеть", None, None, None).unwrap();
        let weather = add(&conn, "Погода в Москве", None, None, None).unwrap();
        let warehouse = add(&conn, "аптечная база на складе", None, None, None).unwrap();
        let (state, _events) = tool_state(conn);
        let ctx = forget_ctx(&state);

        let r = ForgetTool.execute(&json!({ "query": "погода" }), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["action"], "deleted");
        assert_eq!(data["id"], weather.id);

        let r = ForgetTool.execute(&json!({ "query": "аптечная" }), &ctx).unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains(&format!("{}:", clinic.id)), "{:?}", err.message);
        assert!(err.message.contains(&format!("{}:", warehouse.id)), "{:?}", err.message);

        let r = ForgetTool.execute(&json!({ "query": "нет таких слов" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");

        let r = ForgetTool.execute(&json!({}), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        let r = ForgetTool.execute(&json!({ "query": "   " }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn forget_registered_in_all_three_catalogs_as_confirmed_mutating() {
        for catalog in [
            crate::agent::project_tools::project_catalog(),
            crate::harness::harness_catalog(),
            crate::agent::notes_tools::notes_catalog(),
        ] {
            let Resolution::Found(tool) = catalog.resolve("forget") else {
                panic!("forget missing from catalog");
            };
            assert_eq!(tool.class(), ToolClass::Mutating);
            assert_eq!(tool.policy(), PermissionPolicy::Check);
            assert!(tool.danger_reason(&json!({ "id": 1 }), &ToolCtx::default()).is_some());
            assert!(tool
                .danger_reason(&json!({ "query": "кофе", "replacement": "чай" }), &ToolCtx::default())
                .is_some());
            let schema = tool.parameters();
            assert_eq!(schema["additionalProperties"], false);
            for prop in ["id", "query", "replacement"] {
                assert!(schema["properties"][prop].is_object(), "missing property {prop}");
            }
        }
    }

    #[test]
    fn identity_block_is_stable_and_records_are_query_specific() {
        let (wd, conn) = setup("mem-block");
        add(&conn, "Любит кофе по утрам", None, None, None).unwrap();
        add(&conn, "Совсем про другое", None, None, None).unwrap();

        // The system-prompt block carries only the fixed identity lines and
        // the always-injected gender line: identical for any query.
        let block = identity_block(&conn).unwrap();
        assert!(block.starts_with(&format!(
            "{MEMORY_BLOCK_PREFIX}\nYour name is Project-S.\nAddress the user informally with «ты».\nUser gender: unknown (use gender-neutral phrasing until it is known)."
        )));
        assert!(!block.contains("Любит кофе"), "query records leaked into the stable block");
        assert_eq!(identity_block(&conn).unwrap(), block, "identity block must be byte-stable");

        // Query-relevant records live in their own section for the volatile block.
        let records = relevant_records(&conn, None, "кофе").unwrap();
        assert!(records.starts_with(RECORDS_HEADER), "{records}");
        assert!(records.contains("Любит кофе по утрам"));
        assert!(!records.contains("Совсем про другое"));
        assert!(relevant_records(&conn, None, "нет таких слов").unwrap().is_empty());

        // the identity record never rides the volatile section either
        set_user_gender(&conn, "male").unwrap();
        let records = relevant_records(&conn, None, "gender").unwrap();
        assert!(!records.contains("User gender"), "identity record must surface only via the stable block");

        // a known gender lands in the next identity block as an agreement hint
        let block = identity_block(&conn).unwrap();
        assert!(block.contains("User gender: male (use masculine Russian agreement)."));
        assert_eq!(block.matches("User gender").count(), 1, "gender line injected once");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn relevant_records_marks_current_project_and_caps_records_and_chars() {
        let (wd, conn) = setup("mem-block-cap");
        let project = projects::create_project(&conn, &wd, "proj").unwrap();
        add(&conn, "факт про этот проект кофе", Some(project.id), None, None).unwrap();
        for i in 0..30 {
            add(&conn, &format!("лишняя запись {i} кофе"), None, None, None).unwrap();
        }

        let records = relevant_records(&conn, Some(project.id), "кофе").unwrap();
        assert!(records.contains("факт про этот проект кофе [this project]"));
        assert!(records.lines().count() <= 1 + MAX_BLOCK_RECORDS, "record cap exceeded");
        assert!(records.chars().count() <= RECORDS_HEADER.chars().count() + 1 + MAX_BLOCK_CHARS);

        // the same record is not marked for a different (or no) project
        let records = relevant_records(&conn, None, "факт про этот").unwrap();
        assert!(records.contains("факт про этот проект кофе"));
        assert!(!records.contains("[this project]"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
