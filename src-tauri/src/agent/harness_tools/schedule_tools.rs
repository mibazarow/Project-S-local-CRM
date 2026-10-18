// Scheduler tools (SPEC 5.2 #5, 5.3): the agent schedules its own future
// runs. schedule_create binds to the session that created it (ToolCtx
// session_id); a notify schedule needs no session. create/delete are
// confirmed because they make the agent run (or stop) unattended later.
// Every mutation notifies the open Harness view through the run state's
// emitter, mirroring the schedule_* commands in lib.rs.
use crate::agent::project_tools::{envelope, state_of, with_conn};
use crate::agent::result::ToolResult;
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::scheduler::{self, CreateIn};
use crate::sync_util::lock_recover;
use serde_json::{json, Value};

fn emit_schedules_changed(ctx: &ToolCtx) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = lock_recover(&state.emitter).as_ref() {
            emitter.emit_event("schedules-changed", json!({}));
        }
    }
}

pub struct ScheduleCreateTool;

impl Tool for ScheduleCreateTool {
    fn name(&self) -> &str {
        "schedule_create"
    }
    fn description(&self) -> &str {
        "Create a schedule: kind=run_at (one-shot ISO datetime in spec), cron (5-field expression in spec), or notify (native notification, no model call). prompt is the user turn for run_at/cron and the body for notify; timezone is IANA (default: system); title is a short label for the UI (derived from the prompt when absent). Fires while the app is open."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["run_at", "cron", "notify"] },
                "spec": { "type": "string" },
                "prompt": { "type": "string" },
                "timezone": { "type": "string" },
                "title": { "type": "string" }
            },
            "required": ["kind", "spec"],
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
        Some(format!(
            "создание расписания {}: агент будет запускаться сам",
            args.get("kind").and_then(Value::as_str).unwrap_or("?")
        ))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let kind = args["kind"].as_str().unwrap_or_default();
        let spec = args["spec"].as_str().unwrap_or_default();
        let prompt = args.get("prompt").and_then(Value::as_str);
        let timezone = args.get("timezone").and_then(Value::as_str);
        let title = args.get("title").and_then(Value::as_str);
        let r = with_conn(ctx, |conn| {
            let session_id = if kind == "notify" {
                ctx.session_id
            } else {
                match ctx.session_id {
                    Some(id) => Some(id),
                    None => Some(crate::harness::create_session(conn, Some("schedule"))?.id),
                }
            };
            scheduler::create(
                conn,
                &CreateIn {
                    kind,
                    spec,
                    prompt,
                    timezone,
                    session_id,
                    related_task_id: None,
                    title,
                },
                scheduler::now_utc(),
            )
        });
        if r.is_ok() {
            emit_schedules_changed(ctx);
        }
        Ok(envelope(r))
    }
}

pub struct ScheduleListTool;

impl Tool for ScheduleListTool {
    fn name(&self) -> &str {
        "schedule_list"
    }
    fn description(&self) -> &str {
        "List all schedules with title, kind, spec, prompt, status, timezone and next run."
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
        let r = with_conn(ctx, scheduler::list);
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "schedules": list })).capped(self.max_result_len()),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

pub struct SchedulePauseTool;

impl Tool for SchedulePauseTool {
    fn name(&self) -> &str {
        "schedule_pause"
    }
    fn description(&self) -> &str {
        "Pause a schedule by id (reversible; it stops firing until resumed)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "number" } },
            "required": ["id"],
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
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| scheduler::pause(conn, id));
        if r.is_ok() {
            emit_schedules_changed(ctx);
        }
        Ok(envelope(r))
    }
}

pub struct ScheduleDeleteTool;

impl Tool for ScheduleDeleteTool {
    fn name(&self) -> &str {
        "schedule_delete"
    }
    fn description(&self) -> &str {
        "Delete a schedule by id. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "number" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление расписания".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| scheduler::delete(conn, id));
        if r.is_ok() {
            emit_schedules_changed(ctx);
        }
        Ok(match r {
            Ok(_) => ToolResult::ok(json!({ "deleted": id })),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::{exec, setup};
    use rusqlite::params;
    use serde_json::json;
    use std::fs;

    #[test]
    fn schedule_tools_create_list_pause_delete_round_trip() {
        let env = setup("ht-schedule");
        let r = exec(
            "schedule_create",
            json!({ "kind": "cron", "spec": "0 10 * * *", "prompt": "daily", "timezone": "Europe/Moscow" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let id = r.data.unwrap()["id"].as_i64().unwrap();

        // no session in ctx: a dedicated harness session was created for it
        let session_id: Option<i64> = with_conn(&env.ctx, |conn| {
            conn.query_row("SELECT session_id FROM schedules WHERE id = ?1", params![id], |r| r.get(0))
                .map_err(Into::into)
        })
        .unwrap();
        assert!(session_id.is_some(), "run_at/cron schedule must bind to a session");

        let r = exec("schedule_list", json!({}), &env.ctx);
        let items = r.data.unwrap()["schedules"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kind"], "cron");
        assert_eq!(items[0]["title"], "daily", "title derived from the prompt");
        assert!(items[0]["nextRunAt"].is_string());

        let r = exec("schedule_pause", json!({ "id": id }), &env.ctx);
        assert_eq!(r.data.unwrap()["status"], "paused");

        let r = exec("schedule_delete", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let r = exec("schedule_pause", json!({ "id": id }), &env.ctx);
        assert_eq!(r.error.unwrap().code, "not_found");

        // notify needs no session and takes an ISO spec
        let r = exec(
            "schedule_create",
            json!({ "kind": "notify", "spec": "2030-01-01T09:00:00", "prompt": "ping", "timezone": "Europe/Moscow" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let r = exec("schedule_list", json!({}), &env.ctx);
        let items = r.data.unwrap()["schedules"].as_array().unwrap().clone();
        assert_eq!(items[0]["sessionId"], Value::Null);

        // every successful schedule mutation notifies the UI; the failed pause does not
        let events = env.events.lock().unwrap().clone();
        let notified = events
            .iter()
            .filter(|(name, _)| name == "schedules-changed")
            .count();
        assert_eq!(notified, 4, "create/pause/delete/create-notify must notify: {events:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
