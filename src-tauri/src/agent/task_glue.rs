// Shared glue for the duplicated tool pairs (SPEC 4.6 / 5.2 #4): the project
// `task_*` tools (project_tools) and the harness `crm_task_*` twins
// (harness_tools) share their JSON schemas and execution bodies; the only
// difference is how the project is selected (bound ToolCtx project vs
// project_id/project_name args). The `datas_read`/`datas_add` twins share the
// datas.md domain logic the same way. Tool names, descriptions and schemas
// stay byte-identical to the pre-dedup implementations (pinned by tests).
use crate::error::{AppError, Result};
use crate::{fs_ops, paths, tasks, undo};
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::path::Path;

// Maps domain "not found" faults for the envelope codes (used by the datas
// helpers and the project fs tools).
pub(crate) fn is_missing(e: &AppError) -> bool {
    matches!(e, AppError::Invalid(_)) && e.to_string().contains("not found")
}

// Task ids are global in SQLite; the owner lookup backs both the project
// scoping check (require_own_task) and the CRM tools' existence check.
pub(crate) fn task_owner(conn: &Connection, id: i64) -> Result<i64> {
    conn.query_row(
        "SELECT project_id FROM tasks WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )
    .map_err(|_| AppError::Invalid(format!("task {id} not found")))
}

// Optional "checklist" argument as Vec<ChecklistItem>; absent -> None.
pub(crate) fn parse_checklist_arg(
    args: &Value,
) -> std::result::Result<Option<Vec<tasks::ChecklistItem>>, String> {
    match args.get("checklist") {
        None => Ok(None),
        Some(value) => serde_json::from_value::<Vec<tasks::ChecklistItem>>(value.clone())
            .map(Some)
            .map_err(|e| format!("checklist must be an array of {{text, done?}}: {e}")),
    }
}

// TaskPatch ignores the extra selector keys; its opt fields keep absent
// fields unchanged and treat explicit null as "clear".
pub(crate) fn parse_task_patch(args: &Value) -> std::result::Result<tasks::TaskPatch, String> {
    serde_json::from_value(args.clone()).map_err(|e| e.to_string())
}

// --- shared JSON schemas ----------------------------------------------------

const STATUS_ENUM: [&str; 3] = ["new", "progress", "done"];
const RECURRENCE_ENUM: [&str; 4] = ["none", "daily", "weekly", "monthly"];

fn object_schema(props: Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false
    })
}

// The CRM variants address the project by id or name; the project variants
// use the bound project context and carry no selector properties.
fn with_project_selector(mut props: Map<String, Value>) -> Map<String, Value> {
    props.insert("project_id".to_owned(), json!({ "type": "number" }));
    props.insert("project_name".to_owned(), json!({ "type": "string" }));
    props
}

fn checklist_prop() -> Value {
    json!({
        "type": "array",
        "maxItems": 100,
        "items": {
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "done": { "type": "boolean" }
            },
            "required": ["text"],
            "additionalProperties": false
        }
    })
}

pub(crate) fn list_schema(with_selector: bool) -> Value {
    let mut props = Map::new();
    props.insert(
        "status".to_owned(),
        json!({ "type": "string", "enum": STATUS_ENUM }),
    );
    object_schema(
        if with_selector {
            with_project_selector(props)
        } else {
            props
        },
        &[],
    )
}

pub(crate) fn create_schema(with_selector: bool) -> Value {
    let mut props = Map::new();
    props.insert("title".to_owned(), json!({ "type": "string" }));
    props.insert(
        "status".to_owned(),
        json!({ "type": "string", "enum": STATUS_ENUM }),
    );
    props.insert("dueDate".to_owned(), json!({ "type": "string" }));
    props.insert("dueTime".to_owned(), json!({ "type": "string" }));
    props.insert("isPriority".to_owned(), json!({ "type": "boolean" }));
    props.insert(
        "recurrence".to_owned(),
        json!({ "type": "string", "enum": RECURRENCE_ENUM }),
    );
    props.insert("checklist".to_owned(), checklist_prop());
    object_schema(
        if with_selector {
            with_project_selector(props)
        } else {
            props
        },
        &["title"],
    )
}

pub(crate) fn update_schema() -> Value {
    let mut props = Map::new();
    props.insert("id".to_owned(), json!({ "type": "number" }));
    props.insert("title".to_owned(), json!({ "type": "string" }));
    props.insert("comment".to_owned(), json!({ "type": ["string", "null"] }));
    props.insert("dueDate".to_owned(), json!({ "type": ["string", "null"] }));
    // due_time is NOT NULL: null/absent keeps the current value.
    props.insert("dueTime".to_owned(), json!({ "type": "string" }));
    props.insert("remindAt".to_owned(), json!({ "type": ["string", "null"] }));
    props.insert("isPriority".to_owned(), json!({ "type": "boolean" }));
    props.insert(
        "recurrence".to_owned(),
        json!({ "type": "string", "enum": RECURRENCE_ENUM }),
    );
    props.insert("checklist".to_owned(), checklist_prop());
    object_schema(props, &["id"])
}

pub(crate) fn move_schema() -> Value {
    let mut props = Map::new();
    props.insert("id".to_owned(), json!({ "type": "number" }));
    props.insert(
        "status".to_owned(),
        json!({ "type": "string", "enum": STATUS_ENUM }),
    );
    props.insert("position".to_owned(), json!({ "type": "number" }));
    object_schema(props, &["id", "status"])
}

pub(crate) fn delete_schema() -> Value {
    let mut props = Map::new();
    props.insert("id".to_owned(), json!({ "type": "number" }));
    object_schema(props, &["id"])
}

// --- shared execution bodies --------------------------------------------------

pub(crate) fn tasks_list_filtered(
    conn: &Connection,
    pid: i64,
    status: Option<&str>,
) -> Result<Vec<tasks::TaskDto>> {
    let all = tasks::tasks_list(conn, pid)?;
    Ok(all
        .into_iter()
        .filter(|t| status.map_or(true, |s| t.status == s))
        .collect())
}

// Create plus the undo entry; the checklist arg is parsed by the caller so a
// malformed one still surfaces as invalid_args before any DB work.
pub(crate) fn task_create_body(
    conn: &Connection,
    pid: i64,
    args: &Value,
    checklist: Option<Vec<tasks::ChecklistItem>>,
) -> Result<tasks::TaskDto> {
    let title = args["title"].as_str().unwrap_or_default();
    let status = args.get("status").and_then(Value::as_str).map(str::to_owned);
    let due_date = args.get("dueDate").and_then(Value::as_str).map(str::to_owned);
    let due_time = args.get("dueTime").and_then(Value::as_str).map(str::to_owned);
    let is_priority = args.get("isPriority").and_then(Value::as_bool);
    let recurrence = args.get("recurrence").and_then(Value::as_str).map(str::to_owned);
    let dto = tasks::task_create_with(conn, pid, title, status, due_date, due_time, is_priority, checklist, recurrence)?;
    let _ = undo::push_task_create(conn, dto.id, pid);
    Ok(dto)
}

pub(crate) fn task_update_body(
    conn: &Connection,
    id: i64,
    patch: tasks::TaskPatch,
) -> Result<tasks::TaskDto> {
    let snapshot = tasks::task_snapshot(conn, id).ok();
    let dto = tasks::task_update(conn, id, patch)?;
    if let Some(snapshot) = snapshot {
        let _ = undo::push_task_snapshot(conn, "task_update", &snapshot, None);
    }
    Ok(dto)
}

// The position default uses the task's owning project, which for the project
// tools equals the bound project (the caller checked ownership first).
pub(crate) fn task_move_body(
    conn: &Connection,
    id: i64,
    status: &str,
    position: Option<f64>,
) -> Result<tasks::TaskDto> {
    let pid = task_owner(conn, id)?;
    let snapshot = tasks::task_snapshot(conn, id).ok();
    let position = match position {
        Some(p) => p,
        None => conn.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks WHERE project_id = ?1 AND status = ?2",
            params![pid, status],
            |row| row.get(0),
        )?,
    };
    let (dto, spawned) = tasks::task_move_capture(conn, id, status, position)?;
    if let Some(snapshot) = snapshot {
        let _ = undo::push_task_snapshot(conn, "task_move", &snapshot, spawned);
    }
    Ok(dto)
}

// Returns the owning project id so each caller can notify the right kanban.
pub(crate) fn task_delete_body(conn: &Connection, id: i64) -> Result<i64> {
    task_owner(conn, id)?;
    let (pid, trash_id) = tasks::task_delete_with_trash(conn, id)?;
    let _ = undo::push_task_delete(conn, trash_id, pid);
    Ok(pid)
}

// --- datas.md domain logic -----------------------------------------------------

// Structured datas.md edit: a section is "## <title>" up to the next "## "
// heading; an existing section is replaced in place, otherwise appended.
pub(crate) fn upsert_section(md: &str, title: &str, body: &str) -> (bool, String) {
    let heading = format!("## {title}");
    let section = format!("{heading}\n\n{}\n\n", body.trim());
    let mut out = String::new();
    let mut updated = false;
    let mut skipping = false;
    for line in md.lines() {
        if skipping {
            if line.starts_with("## ") {
                skipping = false;
            } else {
                continue;
            }
        }
        if !updated && line.trim_end() == heading {
            out.push_str(&section);
            updated = true;
            skipping = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !updated {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&section);
    }
    (updated, out)
}

// datas.md may be missing on hand-created project folders; treat it as empty.
pub(crate) fn read_datas(conn: &Connection, wd: &Path, pid: i64) -> Result<String> {
    match fs_ops::read_text(conn, wd, pid, paths::DATAS_MD) {
        Ok(text) => Ok(text),
        Err(e) if is_missing(&e) => Ok(String::new()),
        Err(e) => Err(e),
    }
}

pub(crate) fn write_datas(
    conn: &Connection,
    wd: &Path,
    pid: i64,
    content: &str,
) -> Result<fs_ops::FsEntry> {
    match fs_ops::write_text(conn, wd, pid, paths::DATAS_MD, content) {
        Err(e) if is_missing(&e) => {
            fs_ops::create_file(conn, wd, pid, "", paths::DATAS_MD)?;
            fs_ops::write_text(conn, wd, pid, paths::DATAS_MD, content)
        }
        other => other,
    }
}

// Shared "title" argument check for both datas_add variants.
pub(crate) fn datas_title_arg(args: &Value) -> std::result::Result<String, String> {
    let title = args["title"].as_str().unwrap_or_default().trim().to_owned();
    if title.is_empty() || title.starts_with('#') || title.contains('\n') {
        return Err("title must be a single plain line".to_owned());
    }
    Ok(title)
}

// Read the project's datas.md, upsert one section, write it back.
pub(crate) fn datas_add_body(
    conn: &Connection,
    wd: &Path,
    pid: i64,
    title: &str,
    body: &str,
) -> Result<(bool, fs_ops::FsEntry)> {
    let current = read_datas(conn, wd, pid)?;
    let (updated, new_md) = upsert_section(&current, title, body);
    write_datas(conn, wd, pid, &new_md).map(|entry| (updated, entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The generated schemas must stay byte-identical to the literals the two
    // tool sets declared before the dedup (serde_json maps are sorted, so
    // property order does not matter).
    #[test]
    fn task_schemas_match_the_pre_dedup_literals() {
        assert_eq!(
            list_schema(false),
            json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["new", "progress", "done"] }
                },
                "required": [],
                "additionalProperties": false
            })
        );
        assert_eq!(
            list_schema(true),
            json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "number" },
                    "project_name": { "type": "string" },
                    "status": { "type": "string", "enum": ["new", "progress", "done"] }
                },
                "required": [],
                "additionalProperties": false
            })
        );
        assert_eq!(
            create_schema(false),
            json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "status": { "type": "string", "enum": ["new", "progress", "done"] },
                    "dueDate": { "type": "string" },
                    "dueTime": { "type": "string" },
                    "isPriority": { "type": "boolean" },
                    "recurrence": { "type": "string", "enum": ["none", "daily", "weekly", "monthly"] },
                    "checklist": {
                        "type": "array",
                        "maxItems": 100,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string" },
                                "done": { "type": "boolean" }
                            },
                            "required": ["text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["title"],
                "additionalProperties": false
            })
        );
        let mut crm_create = create_schema(false);
        crm_create["properties"]["project_id"] = json!({ "type": "number" });
        crm_create["properties"]["project_name"] = json!({ "type": "string" });
        assert_eq!(create_schema(true), crm_create);
        assert_eq!(
            update_schema(),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "number" },
                    "title": { "type": "string" },
                    "comment": { "type": ["string", "null"] },
                    "dueDate": { "type": ["string", "null"] },
                    "dueTime": { "type": "string" },
                    "remindAt": { "type": ["string", "null"] },
                    "isPriority": { "type": "boolean" },
                    "recurrence": { "type": "string", "enum": ["none", "daily", "weekly", "monthly"] },
                    "checklist": {
                        "type": "array",
                        "maxItems": 100,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string" },
                                "done": { "type": "boolean" }
                            },
                            "required": ["text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            })
        );
        assert_eq!(
            move_schema(),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "number" },
                    "status": { "type": "string", "enum": ["new", "progress", "done"] },
                    "position": { "type": "number" }
                },
                "required": ["id", "status"],
                "additionalProperties": false
            })
        );
        assert_eq!(
            delete_schema(),
            json!({
                "type": "object",
                "properties": { "id": { "type": "number" } },
                "required": ["id"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn upsert_section_updates_in_place_and_appends_new_titles() {
        let (updated, md) = upsert_section("", "FTP", "login: a");
        assert!(!updated);
        assert!(md.contains("## FTP"));
        let (updated, md) = upsert_section(&md, "FTP", "login: b");
        assert!(updated);
        assert_eq!(md.matches("## FTP").count(), 1);
        assert!(md.contains("login: b") && !md.contains("login: a"));
        let (updated, md) = upsert_section(&md, "Admin", "url");
        assert!(!updated);
        assert_eq!(md.matches("## ").count(), 2);
    }

    #[test]
    fn datas_title_arg_rejects_heading_like_and_multiline_titles() {
        assert_eq!(
            datas_title_arg(&json!({ "title": "  FTP example.ru  " })).unwrap(),
            "FTP example.ru"
        );
        assert!(datas_title_arg(&json!({ "title": "  " })).is_err());
        assert!(datas_title_arg(&json!({ "title": "# h" })).is_err());
        assert!(datas_title_arg(&json!({ "title": "a\nb" })).is_err());
    }
}
