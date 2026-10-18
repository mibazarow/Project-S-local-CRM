// Entity links (links slice): relationships between entities recorded in the
// background — no UI. Rows are written automatically after a successful agent
// tool call (chat ↔ file/task/note/schedule/project) and after frontend
// context actions (note ↔ file/chat, task ↔ chat through the link_create
// command). The UNIQUE(from_kind, from_id, to_kind, to_id) constraint is the
// dedupe key: every write is INSERT OR IGNORE, so re-recording is cheap and
// link creation must never fail the operation that triggered it.
use crate::agent::project_tools::{envelope, with_conn};
use crate::agent::result::ToolErrorCode;
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx, ToolResult};
use crate::error::{AppError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

pub const KINDS: [&str; 8] = [
    "project",
    "file",
    "task",
    "note",
    "chat",
    "schedule",
    "objective",
    "message",
];

pub fn is_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

// File ids carry the owning project ("<projectId>:<relPath>") so the same
// relative path in two projects stays two entities; note ids are the bare
// vault-relative path; every other kind uses its numeric row id as text.
pub fn file_id(project_id: i64, rel_path: &str) -> String {
    format!("{project_id}:{rel_path}")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkDto {
    pub id: i64,
    pub from_kind: String,
    pub from_id: String,
    pub to_kind: String,
    pub to_id: String,
    pub created_at: String,
}

const COLS: &str = "id, from_kind, from_id, to_kind, to_id, created_at";

fn dto_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LinkDto> {
    Ok(LinkDto {
        id: row.get(0)?,
        from_kind: row.get(1)?,
        from_id: row.get(2)?,
        to_kind: row.get(3)?,
        to_id: row.get(4)?,
        created_at: row.get(5)?,
    })
}

// INSERT OR IGNORE: recording the same pair twice is a no-op, and a
// self-link is dropped before it can pollute related().
pub fn create(
    conn: &Connection,
    from_kind: &str,
    from_id: &str,
    to_kind: &str,
    to_id: &str,
) -> Result<()> {
    if !is_kind(from_kind) || !is_kind(to_kind) {
        return Err(AppError::Invalid(format!(
            "unknown link kind: {}",
            if !is_kind(from_kind) { from_kind } else { to_kind }
        )));
    }
    if from_kind == to_kind && from_id == to_id {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO links (from_kind, from_id, to_kind, to_id, created_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now','localtime'))",
        params![from_kind, from_id, to_kind, to_id],
    )?;
    Ok(())
}

// Deletes one directed link; true when a row was removed.
#[cfg(test)]
pub fn remove(
    conn: &Connection,
    from_kind: &str,
    from_id: &str,
    to_kind: &str,
    to_id: &str,
) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM links WHERE from_kind = ?1 AND from_id = ?2 AND to_kind = ?3 AND to_id = ?4",
        params![from_kind, from_id, to_kind, to_id],
    )? > 0)
}

// Outgoing links of one entity.
#[cfg(test)]
pub fn list_for(conn: &Connection, kind: &str, id: &str) -> Result<Vec<LinkDto>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM links WHERE from_kind = ?1 AND from_id = ?2 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![kind, id], dto_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// Both directions: every row where the entity sits on either side.
pub fn related(conn: &Connection, kind: &str, id: &str) -> Result<Vec<LinkDto>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM links
         WHERE (from_kind = ?1 AND from_id = ?2) OR (to_kind = ?1 AND to_id = ?2)
         ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![kind, id], dto_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// The entity on the far side of a link, seen from (kind, id).
pub fn other_side<'a>(link: &'a LinkDto, kind: &str, id: &str) -> (&'a str, &'a str) {    if link.from_kind == kind && link.from_id == id {
        (link.to_kind.as_str(), link.to_id.as_str())
    } else {
        (link.from_kind.as_str(), link.from_id.as_str())
    }
}

// Best-effort display title for a linked entity; empty when the row is gone
// (deleted entities leave stale links, which callers simply label by id).
pub fn resolve_title(conn: &Connection, kind: &str, id: &str) -> Result<String> {
    let query = match kind {
        "project" => "SELECT name FROM projects WHERE id = ?1",
        "task" => "SELECT title FROM tasks WHERE id = ?1",
        "schedule" => "SELECT COALESCE(title, '') FROM schedules WHERE id = ?1",
        "objective" => "SELECT title FROM objectives WHERE id = ?1",
        "chat" => "SELECT COALESCE(c.title, s.title, '') FROM chats c
                   LEFT JOIN sessions s ON s.id = c.session_id WHERE c.id = ?1",
        "message" => "SELECT COALESCE(substr(content, 1, 80), '') FROM messages WHERE id = ?1",
        "file" => {
            return Ok(id
                .split_once(':')
                .map(|(_, rel)| rel.to_string())
                .unwrap_or_else(|| id.to_string()));
        }
        "note" => return Ok(id.to_string()),
        _ => return Ok(String::new()),
    };
    let title: Option<String> = conn
        .query_row(query, params![id], |r| r.get(0))
        .optional()?;
    Ok(title.unwrap_or_default())
}

// One short line for the volatile [Context] block: the entities linked to
// (kind, id), titles included. Empty string when nothing is linked, so
// volatile_block drops the section entirely.
pub fn context_line(conn: &Connection, kind: &str, id: &str, max: usize) -> String {
    let links = match related(conn, kind, id) {
        Ok(links) => links,
        Err(_) => return String::new(),
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut items: Vec<String> = Vec::new();
    for link in &links {
        let (k, i) = other_side(link, kind, id);
        if (k, i) == (kind, id) || !seen.insert((k, i)) {
            continue;
        }
        if items.len() >= max {
            break;
        }
        let title = resolve_title(conn, k, i).unwrap_or_default();
        items.push(if title.is_empty() {
            format!("{k} {i}")
        } else {
            format!("{k} {i} «{title}»")
        });
    }
    if items.is_empty() {
        return String::new();
    }
    format!("Related: {}", items.join(", "))
}

fn non_empty_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

// The id a create-style tool returns in its success envelope (data.id).
fn envelope_id(envelope: &Value) -> Option<String> {
    envelope
        .get("data")?
        .get("id")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .map(|v| v.to_string())
}

// Maps one successful tool call to entity links and records them. Only
// called when the tool succeeded and the run has a chat; every failure is
// swallowed — link bookkeeping must never change a tool result.
pub fn auto_link_tool_call(
    conn: &Connection,
    chat_id: i64,
    project_id: Option<i64>,
    tool: &str,
    args: &Value,
    envelope: &Value,
) {
    let chat = chat_id.to_string();
    let link = |kind: &str, id: String| {
        let _ = create(conn, "chat", &chat, kind, &id);
    };
    match tool {
        // Project-scoped file tools: rel_path is already project-relative.
        "read_file" | "list_tree" | "write_file" => {
            let Some(pid) = project_id else { return };
            link("project", pid.to_string());
            if let Some(rel) = non_empty_str(args, "rel_path") {
                link("file", file_id(pid, &rel));
            }
        }
        // path is project-relative for the project agent; the harness agent
        // (no bound project) has no "<pid>:<rel>" id to record.
        "edit_file" | "create_document" | "create_spreadsheet" => {
            let Some(pid) = project_id else { return };
            link("project", pid.to_string());
            if let Some(path) = non_empty_str(args, "path") {
                link("file", file_id(pid, &path));
            }
        }
        "apply_patch" => {
            if let Some(pid) = project_id {
                link("project", pid.to_string());
            }
        }
        "task_create" | "crm_task_create" => {
            if let Some(id) = envelope_id(envelope) {
                link("task", id);
            }
        }
        "task_update" | "task_move" | "task_delete" | "crm_task_update" | "crm_task_move"
        | "crm_task_delete" => {
            if let Some(id) = args.get("id").and_then(Value::as_i64) {
                link("task", id.to_string());
            }
        }
        "notes_read" | "notes_write" | "notes_create" | "notes_rename" | "notes_move"
        | "notes_copy" | "notes_delete" => {
            if let Some(rel) = non_empty_str(args, "relPath") {
                link("note", rel);
            }
        }
        "schedule_create" => {
            if let Some(id) = envelope_id(envelope) {
                link("schedule", id);
            }
        }
        _ => {}
    }
}

// Loop hook: pulls the DB out of the run's ToolCtx and records the links of
// one successful tool call. Silent no-op without a chat or a state handle.
pub fn auto_link_from_tool_ctx(ctx: &ToolCtx, tool: &str, args: &Value, envelope: &Value) {
    let Some(chat_id) = ctx.chat_id else {
        return;
    };
    let Some(state) = &ctx.state else {
        return;
    };
    let Ok(guard) = state.db.lock() else {
        return;
    };
    let Some(conn) = guard.as_ref() else {
        return;
    };
    auto_link_tool_call(conn, chat_id, ctx.project_id, tool, args, envelope);
}

// Agent tool: read the links of one entity. ReadOnly/Skip — the agent can
// always ask what a chat, file, task or note is connected to.
pub struct RelatedTool;

impl Tool for RelatedTool {
    fn name(&self) -> &str {
        "related"
    }
    fn description(&self) -> &str {
        "List entities linked to one entity: kind is project|file|task|note|chat|schedule|objective|message, id is its id (files are \"<projectId>:<relPath>\", notes are the vault path). id absent means the current chat. Returns kind, id and title for every related entity."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": KINDS },
                "id": { "type": "string" }
            },
            "required": ["kind"],
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
        let kind = args["kind"].as_str().unwrap_or_default();
        if !is_kind(kind) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("unknown kind {kind:?}, expected one of {}", KINDS.join(", ")),
            ));
        }
        let id = match args.get("id").and_then(Value::as_str) {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => match ctx.chat_id {
                Some(chat_id) => chat_id.to_string(),
                None => {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        "id is required when the run is not bound to a chat",
                    ))
                }
            },
        };
        let r = with_conn(ctx, |conn| {
            let mut seen = std::collections::BTreeSet::new();
            let mut items: Vec<Value> = Vec::new();
            for link in related(conn, kind, &id)? {
                let (k, i) = other_side(&link, kind, &id);
                let (k, i) = (k.to_string(), i.to_string());
                if (k.as_str(), i.as_str()) == (kind, id.as_str()) || !seen.insert((k.clone(), i.clone())) {
                    continue;
                }
                items.push(json!({
                    "kind": k,
                    "id": i,
                    "title": resolve_title(conn, &k, &i).unwrap_or_default(),
                }));
            }
            Ok(items)
        });
        Ok(match r {
            Ok(items) => ToolResult::ok(json!({ "related": items })).capped(self.max_result_len()),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn conn() -> Connection {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn
    }

    fn link_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM links", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn create_dedupes_through_the_unique_constraint() {
        let conn = conn();
        create(&conn, "chat", "7", "file", "3:a.md").unwrap();
        create(&conn, "chat", "7", "file", "3:a.md").unwrap();
        assert_eq!(link_count(&conn), 1);
        let listed = list_for(&conn, "chat", "7").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].to_id, "3:a.md");
    }

    #[test]
    fn create_rejects_unknown_kinds_and_skips_self_links() {
        let conn = conn();
        assert!(create(&conn, "chat", "7", "bogus", "1").is_err());
        assert!(create(&conn, "thing", "1", "chat", "7").is_err());
        create(&conn, "chat", "7", "chat", "7").unwrap();
        assert_eq!(link_count(&conn), 0);
    }

    #[test]
    fn related_is_bidirectional_and_remove_works() {
        let conn = conn();
        create(&conn, "chat", "7", "task", "12").unwrap();
        let from_chat = related(&conn, "chat", "7").unwrap();
        let from_task = related(&conn, "task", "12").unwrap();
        assert_eq!(from_chat.len(), 1);
        assert_eq!(from_task.len(), 1);
        assert_eq!(other_side(&from_task[0], "task", "12"), ("chat", "7"));
        assert!(remove(&conn, "chat", "7", "task", "12").unwrap());
        assert!(!remove(&conn, "chat", "7", "task", "12").unwrap());
        assert!(related(&conn, "chat", "7").unwrap().is_empty());
    }

    #[test]
    fn auto_links_file_tools_to_chat_and_project() {
        let conn = conn();
        auto_link_tool_call(
            &conn,
            7,
            Some(3),
            "read_file",
            &json!({ "rel_path": "docs/a.md" }),
            &json!({ "ok": true, "data": { "relPath": "docs/a.md" } }),
        );
        auto_link_tool_call(
            &conn,
            7,
            Some(3),
            "edit_file",
            &json!({ "path": "docs/a.md", "old_string": "x", "new_string": "y" }),
            &json!({ "ok": true, "data": { "path": "docs/a.md" } }),
        );
        let related = related(&conn, "chat", "7").unwrap();
        let pairs: Vec<(&str, &str)> = related
            .iter()
            .map(|l| other_side(l, "chat", "7"))
            .collect();
        assert!(pairs.contains(&("project", "3")));
        assert!(pairs.contains(&("file", "3:docs/a.md")));
        assert_eq!(link_count(&conn), 2);
    }

    #[test]
    fn auto_links_task_note_and_schedule() {
        let conn = conn();
        auto_link_tool_call(
            &conn,
            7,
            None,
            "task_create",
            &json!({ "title": "t" }),
            &json!({ "ok": true, "data": { "id": 12 } }),
        );
        auto_link_tool_call(
            &conn,
            7,
            None,
            "task_update",
            &json!({ "id": 13, "title": "t" }),
            &json!({ "ok": true, "data": { "id": 13 } }),
        );
        auto_link_tool_call(
            &conn,
            7,
            None,
            "notes_write",
            &json!({ "relPath": "Заметки/встреча.md", "content": "x" }),
            &json!({ "ok": true, "data": { "relPath": "Заметки/встреча.md" } }),
        );
        auto_link_tool_call(
            &conn,
            7,
            None,
            "schedule_create",
            &json!({ "kind": "run_at", "spec": "2026-11-01T10:00:00" }),
            &json!({ "ok": true, "data": { "id": 4 } }),
        );
        let links = related(&conn, "chat", "7").unwrap();
        let kinds: Vec<(&str, &str)> =
            links.iter().map(|l| other_side(l, "chat", "7")).collect();
        assert_eq!(
            kinds,
            vec![
                ("task", "12"),
                ("task", "13"),
                ("note", "Заметки/встреча.md"),
                ("schedule", "4"),
            ]
        );
    }

    #[test]
    fn auto_links_ignore_unknown_tools_and_missing_ids() {
        let conn = conn();
        auto_link_tool_call(&conn, 7, Some(3), "finish_task", &json!({}), &json!({ "ok": true }));
        auto_link_tool_call(
            &conn,
            7,
            Some(3),
            "task_create",
            &json!({ "title": "t" }),
            &json!({ "ok": true, "data": {} }),
        );
        auto_link_tool_call(
            &conn,
            7,
            None,
            "edit_file",
            &json!({ "path": "/tmp/x.md" }),
            &json!({ "ok": true, "data": { "path": "/tmp/x.md" } }),
        );
        assert_eq!(link_count(&conn), 0);
    }

    #[test]
    fn context_line_lists_related_with_titles_and_caps() {
        let conn = conn();
        assert_eq!(context_line(&conn, "chat", "7", 8), "");
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('example', '/tmp/example', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, created_at) VALUES (1, 'Правка макета', '2026-01-01')",
            [],
        )
        .unwrap();
        create(&conn, "chat", "7", "task", "1").unwrap();
        create(&conn, "file", "1:index.html", "chat", "7").unwrap();
        let line = context_line(&conn, "chat", "7", 8);
        assert!(line.starts_with("Related: "), "{line}");
        assert!(line.contains("task 1 «Правка макета»"), "{line}");
        assert!(line.contains("file 1:index.html «index.html»"), "{line}");
        assert_eq!(context_line(&conn, "chat", "7", 1).matches(", ").count(), 0);
    }

    #[test]
    fn related_tool_is_read_only_and_lists_linked_entities() {
        let conn = conn();
        create(&conn, "chat", "7", "task", "1").unwrap();
        create(&conn, "task", "1", "chat", "7").unwrap(); // duplicate pair deduped in output
        let state = std::sync::Arc::new(crate::state::AppState::new());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            chat_id: Some(7),
            state: Some(state),
            ..Default::default()
        };
        assert_eq!(RelatedTool.class(), ToolClass::ReadOnly);
        assert_eq!(RelatedTool.policy(), PermissionPolicy::Skip);
        let result = RelatedTool
            .execute(&json!({ "kind": "chat" }), &ctx)
            .unwrap();
        assert!(result.ok, "{:?}", result.error);
        let items = result.data.unwrap()["related"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], json!("task"));
        assert_eq!(items[0]["id"], json!("1"));
        // Unknown kind is an invalid_args envelope, not a crash.
        let bad = RelatedTool.execute(&json!({ "kind": "bogus" }), &ctx).unwrap();
        assert_eq!(bad.error.as_ref().unwrap().code, "invalid_args");
    }
}
