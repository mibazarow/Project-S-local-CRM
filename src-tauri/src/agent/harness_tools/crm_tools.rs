// Harness CRM tools (SPEC 5.2 item 4): crm_* task/project tools plus the
// datas_* pair. They address projects by id or exact name. Cross-project
// search iterates all projects: the UI keeps search per-project (SPEC 4.4),
// but the harness agent explicitly gets CRM-wide tools.
use crate::agent::project_tools::{
    emit_tasks_changed, emit_trash_changed, envelope, require_wd, state_of, with_conn,
};
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::task_glue::{
    create_schema, datas_add_body, datas_title_arg, delete_schema, list_schema, move_schema,
    parse_checklist_arg, parse_task_patch, read_datas, task_create_body, task_delete_body,
    task_move_body, task_owner, task_update_body, tasks_list_filtered, update_schema,
};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::envelope_r;
use crate::error::{AppError, Result};
use crate::{db, embed, paths, projects, search, tasks};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

// Harness CRM tools address projects by id or exact name.
fn resolve_project_id(conn: &Connection, args: &Value) -> Result<i64> {
    if let Some(id) = args.get("project_id").and_then(Value::as_i64) {
        let exists: i64 =
            conn.query_row("SELECT count(*) FROM projects WHERE id = ?1", params![id], |r| r.get(0))?;
        if exists == 0 {
            return Err(AppError::Invalid(format!("project {id} not found")));
        }
        return Ok(id);
    }
    if let Some(name) = args.get("project_name").and_then(Value::as_str) {
        return conn
            .query_row(
                "SELECT id FROM projects WHERE name = ?1 COLLATE NOCASE",
                params![name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| AppError::Invalid(format!("project \"{name}\" not found")));
    }
    Err(AppError::Invalid("project_id or project_name is required".into()))
}

pub struct CrmListProjectsTool;

impl Tool for CrmListProjectsTool {
    fn name(&self) -> &str {
        "crm_list_projects"
    }
    fn description(&self) -> &str {
        "List all CRM projects with id, name, path and counts (files, tasks, overdue)."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let r = with_conn(ctx, projects::list_projects);
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "projects": list })).capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct CrmSearchTool;

impl Tool for CrmSearchTool {
    fn name(&self) -> &str {
        "crm_search"
    }
    fn description(&self) -> &str {
        "Hybrid index search (FTS + semantic). With project_id/project_name: one project; without: across all projects (harness-only capability)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "project_id": { "type": "number" },
                "project_name": { "type": "string" },
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
        let state = state_of(ctx)?;
        let query = args["query"].as_str().unwrap_or_default();
        let limit = args
            .get("limit")
            .and_then(Value::as_f64)
            .unwrap_or(10.0)
            .clamp(1.0, 50.0) as usize;
        let scoped = args.get("project_id").is_some() || args.get("project_name").is_some();
        // S1.2: one connection serves the whole call (project resolution plus
        // every project's search legs), not one per project.
        let wd = state.working_dir()?;
        let conn = db::open(&paths::db_path(&wd))?;
        let embedder = embed::select_embedder(&wd);
        let pids: Vec<i64> = if scoped {
            vec![resolve_project_id(&conn, args)?]
        } else {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut hits: Vec<Value> = Vec::new();
        for pid in pids {
            for hit in search::search_with(&conn, embedder.as_ref(), embed::NAME_WEIGHT, pid, query, limit)? {
                let mut v = serde_json::to_value(&hit).unwrap_or(Value::Null);
                v["projectId"] = json!(pid);
                hits.push(v);
            }
        }
        hits.sort_by(|a, b| {
            b["score"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(ToolResult::ok(json!({ "hits": hits })).capped(self.max_result_len()))
    }
}

pub struct CrmTasksTool;

impl Tool for CrmTasksTool {
    fn name(&self) -> &str {
        "crm_tasks"
    }
    fn description(&self) -> &str {
        "List a project's tasks (project_id or project_name); optional status filter (new|progress|done)."
    }
    fn parameters(&self) -> Value {
        list_schema(true)
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let status = args.get("status").and_then(Value::as_str).map(str::to_owned);
        let r = with_conn(ctx, |conn| {
            let pid = resolve_project_id(conn, args)?;
            tasks_list_filtered(conn, pid, status.as_deref())
        });
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "tasks": list })).capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct CrmTaskCreateTool;

impl Tool for CrmTaskCreateTool {
    fn name(&self) -> &str {
        "crm_task_create"
    }
    fn description(&self) -> &str {
        "Create a task in a project (project_id or project_name); optional status (new|progress|done, default new), dueDate (YYYY-MM-DD), dueTime (HH:MM deadline time, default 07:00), isPriority (true|false, default false), recurrence (none|daily|weekly|monthly, default none), checklist ([{text, done?}], max 100 items)."
    }
    fn parameters(&self) -> Value {
        create_schema(true)
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let checklist = match parse_checklist_arg(args) {
            Ok(items) => items,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let r = with_conn(ctx, |conn| {
            let pid = resolve_project_id(conn, args)?;
            task_create_body(conn, pid, args, checklist)
        });
        if let Ok(dto) = &r {
            emit_tasks_changed(ctx, dto.project_id);
        }
        Ok(envelope(r))
    }
}

pub struct CrmTaskUpdateTool;

impl Tool for CrmTaskUpdateTool {
    fn name(&self) -> &str {
        "crm_task_update"
    }
    fn description(&self) -> &str {
        "Update fields of a task by id; absent fields stay unchanged, explicit null clears comment/dueDate/remindAt. Dates: YYYY-MM-DD, dueTime: HH:MM (null/absent keeps the current value), remindAt: YYYY-MM-DDTHH:MM. isPriority: true|false, recurrence: none|daily|weekly|monthly, checklist: [{text, done?}] max 100 (null/absent keeps the current value)."
    }
    fn parameters(&self) -> Value {
        update_schema()
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let patch: tasks::TaskPatch = match parse_task_patch(args) {
            Ok(p) => p,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg)),
        };
        let r = with_conn(ctx, |conn| {
            task_owner(conn, id)?;
            task_update_body(conn, id, patch)
        });
        if let Ok(dto) = &r {
            emit_tasks_changed(ctx, dto.project_id);
        }
        Ok(envelope(r))
    }
}

pub struct CrmTaskMoveTool;

impl Tool for CrmTaskMoveTool {
    fn name(&self) -> &str {
        "crm_task_move"
    }
    fn description(&self) -> &str {
        "Move a task to a kanban column (new|progress|done); optional position, appended at the column end when absent. Moving a recurring task (recurrence daily/weekly/monthly) to done queues the next occurrence in the new column."
    }
    fn parameters(&self) -> Value {
        move_schema()
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let status = args["status"].as_str().unwrap_or_default();
        let position = args.get("position").and_then(Value::as_f64);
        let r = with_conn(ctx, |conn| task_move_body(conn, id, status, position));
        if let Ok(dto) = &r {
            emit_tasks_changed(ctx, dto.project_id);
        }
        Ok(envelope(r))
    }
}

pub struct CrmTaskDeleteTool;

impl Tool for CrmTaskDeleteTool {
    fn name(&self) -> &str {
        "crm_task_delete"
    }
    fn description(&self) -> &str {
        "Delete a task by id. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        delete_schema()
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление задачи из CRM".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| task_delete_body(conn, id));
        if let Ok(pid) = &r {
            emit_tasks_changed(ctx, *pid);
            emit_trash_changed(ctx, None);
        }
        Ok(match r {
            Ok(_) => ToolResult::ok(json!({ "deleted": id })),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct DatasReadTool;

impl Tool for DatasReadTool {
    fn name(&self) -> &str {
        "datas_read"
    }
    fn description(&self) -> &str {
        "Read any project's datas.md (access notes; project_id or project_name); empty string when the file is missing."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "number" },
                "project_name": { "type": "string" }
            },
            "required": [],
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
        let wd = require_wd(ctx)?;
        let pid = with_conn(ctx, |conn| resolve_project_id(conn, args))?;
        let r = with_conn(ctx, |conn| read_datas(conn, &wd, pid));
        Ok(match r {
            Ok(text) => ToolResult::ok(json!({ "projectId": pid, "relPath": paths::DATAS_MD, "content": text }))
                .capped(self.max_result_len()),
            Err(e) => envelope_r(e),
        })
    }
}

pub struct DatasAddTool;

impl Tool for DatasAddTool {
    fn name(&self) -> &str {
        "datas_add"
    }
    fn description(&self) -> &str {
        "Add or update a \"## <title>\" section in any project's datas.md (project_id or project_name); an existing section is replaced, otherwise appended."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "number" },
                "project_name": { "type": "string" },
                "title": { "type": "string" },
                "body": { "type": "string" }
            },
            "required": ["title", "body"],
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
        let wd = require_wd(ctx)?;
        let pid = with_conn(ctx, |conn| resolve_project_id(conn, args))?;
        let title = match datas_title_arg(args) {
            Ok(title) => title,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg)),
        };
        let body = args["body"].as_str().unwrap_or_default();
        let r = with_conn(ctx, |conn| datas_add_body(conn, &wd, pid, &title, body));
        Ok(match r {
            Ok((updated, entry)) => ToolResult::ok(json!({ "projectId": pid, "title": title, "updated": updated, "entry": entry })),
            Err(e) => envelope_r(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::{exec, setup};
    use crate::index;
    use std::fs;

    #[test]
    fn crm_projects_search_tasks_and_datas_round_trip() {
        let env = setup("ht-crm");
        let (p1, p2) = with_conn(&env.ctx, |conn| {
            let a = projects::create_project(conn, &env.wd, "alpha")?;
            let b = projects::create_project(conn, &env.wd, "beta")?;
            Ok::<(i64, i64), AppError>((a.id, b.id))
        })
        .unwrap();

        let r = exec("crm_list_projects", json!({}), &env.ctx);
        let projects = r.data.unwrap()["projects"].as_array().unwrap().clone();
        assert_eq!(projects.len(), 2);
        assert!(projects[0]["taskCount"].is_number());

        // index one file per project, then search scoped and cross-project
        fs::write(paths::projects_dir(&env.wd).join("alpha/plan.md"), "crossword in alpha").unwrap();
        fs::write(paths::projects_dir(&env.wd).join("beta/notes.md"), "crossword in beta").unwrap();
        let state = env.ctx.state.as_deref().unwrap();
        index::index_project(state, p1).unwrap();
        index::index_project(state, p2).unwrap();

        let r = exec("crm_search", json!({ "query": "crossword", "project_name": "alpha" }), &env.ctx);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["projectId"], p1);
        assert_eq!(hits[0]["relPath"], "plan.md");

        let r = exec("crm_search", json!({ "query": "crossword" }), &env.ctx);
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 2, "cross-project search: {hits:?}");
        let pids: Vec<i64> = hits.iter().map(|h| h["projectId"].as_i64().unwrap()).collect();
        assert!(pids.contains(&p1) && pids.contains(&p2));

        // task round trip
        let r = exec("crm_task_create", json!({ "project_name": "alpha", "title": "first" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        let r = exec("crm_task_update", json!({ "id": id, "title": "renamed", "comment": "note" }), &env.ctx);
        let data = r.data.unwrap();
        assert_eq!(data["title"], "renamed");
        assert_eq!(data["comment"], "note");

        let r = exec("crm_task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert_eq!(r.data.unwrap()["status"], "done");

        let r = exec("crm_tasks", json!({ "project_name": "alpha", "status": "done" }), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);

        let r = exec("crm_task_delete", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("crm_task_update", json!({ "id": id, "title": "ghost" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        // every successful task mutation notifies the UI with the owning project
        let events = env.events.lock().unwrap().clone();
        let notified: Vec<&Value> = events
            .iter()
            .filter(|(name, _)| name == "tasks-changed")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(notified.len(), 4, "create/update/move/delete must notify: {events:?}");
        assert!(
            notified.iter().all(|p| p["projectId"] == json!(p1)),
            "payload must carry the owning project id: {notified:?}"
        );

        // datas for any project
        let r = exec("datas_add", json!({ "project_name": "beta", "title": "FTP example.ru", "body": "login: user-1" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("datas_read", json!({ "project_name": "beta" }), &env.ctx);
        let content = r.data.unwrap()["content"].as_str().unwrap().to_owned();
        assert!(content.contains("## FTP example.ru") && content.contains("user-1"));

        let r = exec("crm_tasks", json!({ "project_name": "nope" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn crm_search_opens_one_connection_for_the_whole_call() {
        // S1.2: the old shape opened one connection per project (search_project
        // per pid); now a single connection serves resolution plus all legs.
        let env = setup("ht-crm-conn");
        let mut pids = Vec::new();
        for name in ["alpha", "beta", "gamma"] {
            let dto = with_conn(&env.ctx, |conn| {
                projects::create_project(conn, &env.wd, name)
            })
            .unwrap();
            pids.push(dto.id);
        }
        for name in ["alpha", "beta", "gamma"] {
            fs::write(
                paths::projects_dir(&env.wd).join(format!("{name}/plan.md")),
                "crossword word",
            )
            .unwrap();
        }
        let state = env.ctx.state.as_deref().unwrap();
        for pid in &pids {
            index::index_project(state, *pid).unwrap();
        }

        db::open_counter::arm();
        let r = exec("crm_search", json!({ "query": "crossword" }), &env.ctx);
        let opens = db::open_counter::stop();
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 3, "cross-project hits: {hits:?}");
        let hit_pids: Vec<i64> = hits
            .iter()
            .map(|h| h["projectId"].as_i64().unwrap())
            .collect();
        assert_eq!(hit_pids.len(), hit_pids.iter().collect::<std::collections::HashSet<_>>().len());
        assert_eq!(opens, 1, "crm_search must open exactly one connection per call");

        db::open_counter::arm();
        let r = exec(
            "crm_search",
            json!({ "query": "crossword", "project_name": "beta" }),
            &env.ctx,
        );
        let opens = db::open_counter::stop();
        let hits = r.data.unwrap()["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["projectId"], pids[1]);
        assert_eq!(hits[0]["relPath"], "plan.md");
        assert_eq!(opens, 1, "scoped crm_search must open exactly one connection");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn crm_task_tools_accept_and_validate_is_priority_due_date_checklist_recurrence() {
        let env = setup("ht-crm-upgrade");
        with_conn(&env.ctx, |conn| {
            projects::create_project(conn, &env.wd, "alpha")?;
            Ok::<(), AppError>(())
        })
        .unwrap();

        let r = exec(
            "crm_task_create",
            json!({
                "project_name": "alpha",
                "title": "report",
                "dueDate": "2099-01-02",
                "isPriority": true,
                "recurrence": "daily",
                "checklist": [{ "text": "draft", "done": true }]
            }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let id = data["id"].as_i64().unwrap();
        assert_eq!(data["isPriority"], true);
        assert_eq!(data["dueDate"], "2099-01-02");
        assert_eq!(data["recurrence"], "daily");
        assert_eq!(data["checklist"], json!([{ "text": "draft", "done": true }]));

        let r = exec("crm_task_update", json!({ "id": id, "isPriority": false }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["isPriority"], false);

        let r = exec("crm_task_create", json!({ "project_name": "alpha", "title": "x", "dueDate": "2099-13-01" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("due_date"));
        let r = exec("crm_task_update", json!({ "id": id, "recurrence": "yearly" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("recurrence"));

        // done move queues the next occurrence; crm_tasks sees both.
        let r = exec(
            "crm_task_update",
            json!({ "id": id, "isPriority": true, "dueDate": "2099-01-02" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let r = exec("crm_task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("crm_tasks", json!({ "project_name": "alpha" }), &env.ctx);
        let tasks = r.data.unwrap()["tasks"].as_array().unwrap().clone();
        assert_eq!(tasks.len(), 2);
        let next = tasks.iter().find(|t| t["status"] == "new").unwrap();
        assert_eq!(next["dueDate"], "2099-01-03");
        assert_eq!(next["isPriority"], true, "the flag must be copied to the next occurrence");
        assert_eq!(next["checklist"], json!([{ "text": "draft", "done": false }]));
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
