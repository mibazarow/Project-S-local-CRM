// Project task tools: task_list, task_create, task_update, task_move,
// task_delete. Task ids are global in SQLite; the project agent may only touch
// its own project's tasks (require_own_task), and a foreign id reports as
// not_found (no existence leak). The shared body/schema helpers live in
// task_glue and are reused by the harness CRM tools.
use super::support::{
    emit_tasks_changed, emit_trash_changed, envelope, fail, require_project, with_conn,
};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::agent::task_glue::{
    create_schema, delete_schema, list_schema, move_schema, parse_checklist_arg, parse_task_patch,
    task_create_body, task_delete_body, task_move_body, task_owner, task_update_body,
    tasks_list_filtered, update_schema,
};
use crate::error::{AppError, Result};
use crate::tasks;
use rusqlite::Connection;
use serde_json::{json, Value};

// Task ids are global in SQLite; the project agent may only touch its own
// project's tasks, and a foreign id reports as not_found (no existence leak).
fn require_own_task(conn: &Connection, id: i64, pid: i64) -> Result<()> {
    if task_owner(conn, id)? == pid {
        Ok(())
    } else {
        Err(AppError::Invalid(format!("task {id} not found")))
    }
}

pub struct TaskListTool;

impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }
    fn description(&self) -> &str {
        "List this project's tasks in kanban order; optional status filter (new|progress|done)."
    }
    fn parameters(&self) -> Value {
        list_schema(false)
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let status = args.get("status").and_then(Value::as_str);
        let r = with_conn(ctx, |conn| tasks_list_filtered(conn, pid, status));
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "tasks": list })).capped(self.max_result_len()),
            Err(e) => fail(e),
        })
    }
}

pub struct TaskCreateTool;

impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }
    fn description(&self) -> &str {
        "Create a task in this project; optional status (new|progress|done, default new), dueDate (YYYY-MM-DD), dueTime (HH:MM deadline time, default 07:00), isPriority (true|false, default false), checklist ([{text, done?}], max 100 items)."
    }
    fn parameters(&self) -> Value {
        create_schema(false)
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let checklist = match parse_checklist_arg(args) {
            Ok(items) => items,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let r = with_conn(ctx, |conn| task_create_body(conn, pid, args, checklist));
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
        }
        Ok(envelope(r))
    }
}

pub struct TaskUpdateTool;

impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }
    fn description(&self) -> &str {
        "Update fields of one of this project's tasks; absent fields stay unchanged, explicit null clears comment/dueDate/remindAt. Dates: YYYY-MM-DD, dueTime: HH:MM (null/absent keeps the current value), remindAt: YYYY-MM-DDTHH:MM. isPriority: true|false, checklist: [{text, done?}] max 100 (null/absent keeps the current value)."
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
        let pid = require_project(ctx)?;
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        // TaskPatch ignores the extra "id" key; its opt fields keep absent
        // fields unchanged and treat explicit null as "clear".
        let patch: tasks::TaskPatch = match parse_task_patch(args) {
            Ok(p) => p,
            Err(msg) => {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
            }
        };
        let r = with_conn(ctx, |conn| {
            require_own_task(conn, id, pid)?;
            task_update_body(conn, id, patch)
        });
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
        }
        Ok(envelope(r))
    }
}

pub struct TaskMoveTool;

impl Tool for TaskMoveTool {
    fn name(&self) -> &str {
        "task_move"
    }
    fn description(&self) -> &str {
        "Move one of this project's tasks to a kanban column (new|progress|done); optional position, appended at the column end when absent."
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
        let pid = require_project(ctx)?;
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let status = args["status"].as_str().unwrap_or_default();
        let position = args.get("position").and_then(Value::as_f64);
        let r = with_conn(ctx, |conn| {
            require_own_task(conn, id, pid)?;
            task_move_body(conn, id, status, position)
        });
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
        }
        Ok(envelope(r))
    }
}

pub struct TaskDeleteTool;

impl Tool for TaskDeleteTool {
    fn name(&self) -> &str {
        "task_delete"
    }
    fn description(&self) -> &str {
        "Delete one of this project's tasks. Dangerous: always confirmed."
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
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let pid = require_project(ctx)?;
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| {
            require_own_task(conn, id, pid)?;
            task_delete_body(conn, id)
        });
        if r.is_ok() {
            emit_tasks_changed(ctx, pid);
            emit_trash_changed(ctx, None);
        }
        Ok(match r {
            Ok(_) => ToolResult::ok(json!({ "deleted": id })),
            Err(e) => fail(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_harness::{exec, setup, tool};
    use crate::agent::tool::validate_args;
    use serde_json::{json, Value};
    use std::fs;

    #[test]
    fn task_tools_round_trip_and_project_scoping() {
        let env = setup("pt-tasks");
        let r = exec("task_create", json!({ "title": "  first  " }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        let r = exec("task_list", json!({}), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);

        let r = exec(
            "task_update",
            json!({ "id": id, "title": "renamed", "comment": "note", "dueDate": "2099-01-02", "isPriority": true }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["title"], "renamed");
        assert_eq!(data["comment"], "note");
        assert_eq!(data["dueDate"], "2099-01-02");
        assert_eq!(data["isPriority"], true);

        // explicit null clears the comment
        let r = exec("task_update", json!({ "id": id, "comment": null }), &env.ctx);
        assert_eq!(r.data.unwrap()["comment"], Value::Null);

        let r = exec("task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.clone().unwrap()["status"], "done");
        assert!(r.data.unwrap()["closedAt"].as_str().is_some());

        let r = exec("task_list", json!({ "status": "done" }), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);
        let r = exec("task_list", json!({ "status": "new" }), &env.ctx);
        assert!(r.data.unwrap()["tasks"].as_array().unwrap().is_empty());

        // foreign task ids are not_found for this project
        let r = exec("task_delete", json!({ "id": 9999 }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        let r = exec("task_delete", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("task_list", json!({}), &env.ctx);
        assert!(r.data.unwrap()["tasks"].as_array().unwrap().is_empty());
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn task_tools_accept_and_validate_is_priority_due_date_checklist() {
        let env = setup("pt-tasks-upgrade");
        let r = exec(
            "task_create",
            json!({
                "title": "report",
                "dueDate": "2099-01-02",
                "isPriority": true,
                "checklist": [{ "text": "draft", "done": true }, { "text": "send" }]
            }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let id = data["id"].as_i64().unwrap();
        assert_eq!(data["isPriority"], true);
        assert_eq!(data["dueDate"], "2099-01-02");
        assert_eq!(
            data["checklist"],
            json!([{ "text": "draft", "done": true }, { "text": "send", "done": false }])
        );

        let r = exec(
            "task_update",
            json!({ "id": id, "isPriority": false, "checklist": [{ "text": "only", "done": true }] }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["isPriority"], false);
        assert_eq!(data["checklist"], json!([{ "text": "only", "done": true }]));

        // Domain validation surfaces as a tool error, not a panic.
        let r = exec("task_create", json!({ "title": "x", "dueDate": "2099-13-01" }), &env.ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("due_date"));
        let r = exec("task_update", json!({ "id": id, "checklist": [{ "nope": 1 }] }), &env.ctx);
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn task_update_edits_in_place_and_round_trips_every_editable_field() {
        let env = setup("pt-task-edit");
        let r = exec(
            "task_create",
            json!({ "title": "call client", "dueDate": "2099-01-02", "dueTime": "09:00" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        let r = exec(
            "task_update",
            json!({
                "id": id,
                "title": "call client again",
                "dueDate": "2099-02-03",
                "dueTime": "14:30",
                "comment": "call after 14:00",
                "remindAt": "2099-02-03T14:00",
                "isPriority": true,
                "checklist": [{ "text": "prep", "done": false }]
            }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["id"], id, "the same task is edited in place");
        assert_eq!(data["title"], "call client again");
        assert_eq!(data["dueDate"], "2099-02-03");
        assert_eq!(data["dueTime"], "14:30");
        assert_eq!(data["comment"], "call after 14:00");
        assert_eq!(data["remindAt"], "2099-02-03T14:00");
        assert_eq!(data["isPriority"], true);
        assert_eq!(data["checklist"], json!([{ "text": "prep", "done": false }]));

        // editing must not create or delete tasks: same id, still exactly one
        let r = exec("task_list", json!({}), &env.ctx);
        let tasks = r.data.unwrap()["tasks"].as_array().unwrap().clone();
        assert_eq!(tasks.len(), 1, "no create/delete side effects");
        assert_eq!(tasks[0]["id"], id);
        assert_eq!(tasks[0]["title"], "call client again");

        // explicit null dueTime keeps the current value (schema accepts null)
        let r = exec("task_update", json!({ "id": id, "dueTime": null }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["dueTime"], "14:30");

        // missing id is rejected by the schema validator
        let err = validate_args(
            &tool("task_update").parameters(),
            &json!({ "title": "no id" }),
        )
        .unwrap_err();
        assert!(err.contains("id"), "{err}");
        // an unknown id errors without touching the board
        let r = exec("task_update", json!({ "id": 9999, "title": "ghost" }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = exec("task_list", json!({}), &env.ctx);
        assert_eq!(r.data.unwrap()["tasks"].as_array().unwrap().len(), 1);

        // both updates are undoable: popping the snapshots restores the pre-edit values
        let r = exec("undo_last", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["undoneKind"], "task_update");
        let r = exec("undo_last", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["undoneKind"], "task_update");
        let r = exec("task_list", json!({}), &env.ctx);
        let tasks = r.data.unwrap()["tasks"].as_array().unwrap().clone();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], id);
        assert_eq!(tasks[0]["title"], "call client");
        assert_eq!(tasks[0]["dueDate"], "2099-01-02");
        assert_eq!(tasks[0]["dueTime"], "09:00");
        assert_eq!(tasks[0]["isPriority"], false);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn task_update_is_registered_with_all_editable_fields_in_its_schema() {
        let specs = super::super::project_catalog().to_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == "task_update")
            .expect("task_update must be registered for the project agent");
        let props = spec.parameters["properties"].as_object().unwrap();
        for field in [
            "id",
            "title",
            "comment",
            "dueDate",
            "dueTime",
            "remindAt",
            "isPriority",
            "checklist",
        ] {
            assert!(props.contains_key(field), "task_update schema must expose {field}");
        }
        assert_eq!(spec.parameters["required"], json!(["id"]));
    }

    #[test]
    fn task_mutations_emit_tasks_changed_for_the_bound_project() {
        let env = setup("pt-task-events");
        let pid = env.ctx.project_id.unwrap();
        let r = exec("task_create", json!({ "title": "live" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();
        exec("task_update", json!({ "id": id, "title": "renamed" }), &env.ctx);
        exec("task_move", json!({ "id": id, "status": "done" }), &env.ctx);
        // a failed mutation must not notify
        exec("task_update", json!({ "id": 9999, "title": "ghost" }), &env.ctx);
        exec("task_delete", json!({ "id": id }), &env.ctx);

        let events = env.events.lock().unwrap().clone();
        let notified: Vec<&Value> = events
            .iter()
            .filter(|(name, _)| name == "tasks-changed")
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(notified.len(), 4, "create/update/move/delete must notify: {events:?}");
        assert!(
            notified.iter().all(|p| p["projectId"] == json!(pid)),
            "payload must carry the bound project id: {notified:?}"
        );
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
