// Global memory (SPEC 6.3): one SQLite store shared by project chats and
// harness sessions. Records carry facts/preferences/lessons; the user's
// gender lives here as a fixed-source identity record, injected into every
// context block. Search is FTS5 keyword search over
// memory_fts (rowid = memory.id) — enough at M1 scale and it works with the
// Noop embedder; no vector leg for memory yet.
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::search;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

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

// Identity is fixed: the agent is always "Project-S" and always addresses
// the user informally with «ты». Only the user's gender is a learned memory
// record (set by the set_user_gender tool), injected into every block.
pub const IDENTITY_GENDER_SOURCE: &str = "identity:gender";
pub const AGENT_NAME: &str = "Project-S";
const GENDER_VALUES: [&str; 3] = ["male", "female", "unknown"];

// Header of the block appended to every run's system prompt.
pub const MEMORY_BLOCK_PREFIX: &str = "[Memory]";

// The injected block stays short: at most this many records and this many
// characters of record lines (the identity line itself is always kept).
const MAX_BLOCK_RECORDS: usize = 10;
const MAX_BLOCK_CHARS: usize = 1_500;

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
    conn.execute(
        "INSERT INTO memory_fts (rowid, content) VALUES (?1, ?2)",
        params![id, content],
    )?;
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
    conn.execute("UPDATE memory_fts SET content = ?1 WHERE rowid = ?2", params![content, id])?;
    get(conn, id)?.ok_or_else(|| AppError::Db(format!("memory {id} vanished after update")))
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let deleted = conn.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![id])?;
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

// Keyword search via the FTS mirror; tokens are quoted phrases so FTS
// operator syntax in user input never survives (same defense as file search).
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<MemoryDto>> {
    search_preferring(conn, query, limit, None)
}

// prefer_project ranks records attached to that project first: they are the
// most relevant for that chat (SPEC 6.3 "подсвечиваются в его чате").
fn search_preferring(
    conn: &Connection,
    query: &str,
    limit: usize,
    prefer_project: Option<i64>,
) -> Result<Vec<MemoryDto>> {
    let terms = search::tokenize(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content, m.project_id, m.tags, m.source, m.created_at, m.updated_at
         FROM memory_fts f JOIN memory m ON m.id = f.rowid
         WHERE memory_fts MATCH ?1
         ORDER BY (m.project_id IS ?2) DESC, rank LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![search::fts_match_query(&terms), prefer_project, limit as i64],
        dto_from_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
            let tags_json = serde_json::to_string(&normalize_tags(tags.to_vec()))?;
            conn.execute(
                "UPDATE memory SET content = ?1, tags = ?2, updated_at = datetime('now','localtime') WHERE id = ?3",
                params![content, tags_json, id],
            )?;
            conn.execute("UPDATE memory_fts SET content = ?1 WHERE rowid = ?2", params![content, id])?;
        }
        None => {
            add(conn, content, None, Some(tags.to_vec()), Some(source))?;
        }
    }
    Ok(())
}

// Builds the "[Memory]" block for one run: the fixed identity lines, the
// always-injected gender line, then records relevant to the fresh user query
// (keyword search — works with the Noop embedder). Records attached to the
// current project are marked for that chat (SPEC 6.3 "подсвечиваются").
pub fn context_block(conn: &Connection, project_id: Option<i64>, query: &str) -> Result<String> {
    let gender = user_gender(conn)?;
    let mut block = format!("{MEMORY_BLOCK_PREFIX}\n{}\n{}", identity_lines(), gender_line(&gender));

    let mut records: Vec<MemoryDto> = Vec::new();
    for record in search_preferring(conn, query, MAX_BLOCK_RECORDS * 2, project_id)? {
        if records.len() >= MAX_BLOCK_RECORDS {
            break;
        }
        if record.source.as_deref().is_some_and(|s| s.starts_with("identity:")) {
            continue;
        }
        records.push(record);
    }

    let mut chars = block.chars().count();
    for record in records {
        let mut line = format!("\n- {}", record.content.replace('\n', " "));
        if record.project_id.is_some() && record.project_id == project_id {
            line.push_str(" [this project]");
        }
        if chars + line.chars().count() > MAX_BLOCK_CHARS {
            break;
        }
        chars += line.chars().count();
        block.push_str(&line);
    }
    Ok(block)
}

// Borrows the shared app-state connection for a tool call (same pattern as
// the project tools).
fn db_guard(ctx: &ToolCtx) -> Result<std::sync::MutexGuard<'_, Option<Connection>>> {
    let state = ctx.state.as_deref().ok_or(AppError::NotConfigured)?;
    Ok(state.db.lock().unwrap())
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

// recall: read-only keyword search over the same global store.
pub struct RecallTool;

impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        "Search the global memory (facts, preferences, decisions, lessons shared across all chats and sessions) by keywords."
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Resolution;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use crate::state::AppState;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    #[test]
    fn crud_round_trip_keeps_tags_and_fts_in_sync() {
        let (wd, conn) = setup("mem-crud");
        let dto = add(&conn, "предпочитает короткие ответы", None, Some(vec!["pref".into(), " ".into()]), Some("user")).unwrap();
        assert_eq!(dto.tags, vec!["pref"]);
        assert_eq!(dto.source.as_deref(), Some("user"));
        assert_eq!(list(&conn).unwrap().len(), 1);

        let updated = update(&conn, dto.id, "предпочитает очень короткие ответы", Some(vec![])).unwrap();
        assert_eq!(updated.content, "предпочитает очень короткие ответы");
        assert!(updated.tags.is_empty());

        assert!(matches!(update(&conn, 999, "x", None), Err(AppError::Invalid(_))));
        assert!(matches!(add(&conn, "   ", None, None, None), Err(AppError::Invalid(_))));

        delete(&conn, dto.id).unwrap();
        assert!(matches!(delete(&conn, dto.id), Err(AppError::Invalid(_))));
        assert!(list(&conn).unwrap().is_empty());
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
        assert!(search(&conn, "OR NEAR \"", 10).unwrap().is_empty());
        assert!(search(&conn, "", 10).unwrap().is_empty());
        // keyword search needs no embedder: the Noop default finds records too
        fs::remove_dir_all(&wd).unwrap();
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
            project_id: None,
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

    #[test]
    fn context_block_has_fixed_identity_and_unknown_gender_by_default() {
        let (wd, conn) = setup("mem-block");
        add(&conn, "Любит кофе по утрам", None, None, None).unwrap();
        add(&conn, "Совсем про другое", None, None, None).unwrap();

        let block = context_block(&conn, None, "кофе").unwrap();
        assert!(block.starts_with(&format!(
            "{MEMORY_BLOCK_PREFIX}\nYour name is Project-S.\nAddress the user informally with «ты».\nUser gender: unknown (use gender-neutral phrasing until it is known)."
        )));
        assert!(block.contains("Любит кофе по утрам"));
        assert!(!block.contains("Совсем про другое"));

        // a known gender lands in the next block as an agreement hint
        set_user_gender(&conn, "female").unwrap();
        let block = context_block(&conn, None, "кофе").unwrap();
        assert!(block.contains("User gender: female (use feminine Russian agreement)."));
        assert_eq!(block.matches("User gender").count(), 1, "gender line injected once");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn context_block_marks_current_project_and_caps_records_and_chars() {
        let (wd, conn) = setup("mem-block-cap");
        let project = projects::create_project(&conn, &wd, "proj").unwrap();
        add(&conn, "факт про этот проект кофе", Some(project.id), None, None).unwrap();
        for i in 0..30 {
            add(&conn, &format!("лишняя запись {i} кофе"), None, None, None).unwrap();
        }

        let block = context_block(&conn, Some(project.id), "кофе").unwrap();
        assert!(block.contains("факт про этот проект кофе [this project]"));
        let header = format!("{MEMORY_BLOCK_PREFIX}\n{}\n{}", identity_lines(), gender_line("unknown"));
        let lines = block.lines().count();
        assert!(lines <= header.lines().count() + MAX_BLOCK_RECORDS, "record cap exceeded: {lines}");
        assert!(block.chars().count() <= header.chars().count() + MAX_BLOCK_CHARS);

        // the same record is not marked for a different (or no) project
        let block = context_block(&conn, None, "факт про этот").unwrap();
        assert!(block.contains("факт про этот проект кофе"));
        assert!(!block.contains("[this project]"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
