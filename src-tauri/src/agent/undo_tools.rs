// Undo tools (SPEC undo slice): expose the bounded undo stack to the agent.
// undo_last pops and reverses the newest mutation; undo_list previews the stack.
use super::project_tools::{envelope, require_wd, state_of, with_conn};
use super::result::ToolResult;
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::undo;
use serde_json::{json, Value};

pub struct UndoLastTool;

impl Tool for UndoLastTool {
    fn name(&self) -> &str {
        "undo_last"
    }

    fn description(&self) -> &str {
        "Отменить последнее обратимое действие."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }

    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }

    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }

    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let out = with_conn(ctx, |conn| undo::undo_last(conn, &wd))?;
        emit_undo_events(ctx, &out);
        Ok(envelope(Ok(out)))
    }
}

pub struct UndoListTool;

impl Tool for UndoListTool {
    fn name(&self) -> &str {
        "undo_list"
    }

    fn description(&self) -> &str {
        "Список последних отменяемых действий."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
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
        let limit = args.get("limit").and_then(Value::as_u64).map(|v| v as usize);
        Ok(envelope(with_conn(ctx, |conn| undo::list(conn, limit))))
    }
}

fn emit_undo_events(ctx: &ToolCtx, out: &undo::UndoOutcome) {
    let Ok(state) = state_of(ctx) else {
        return;
    };
    let binding = state.emitter.lock().unwrap();
    let Some(emitter) = binding.as_ref() else {
        return;
    };
    if let Some(fs) = &out.fs_event {
        emitter.emit_event(
            "fs-changed",
            json!({ "projectId": fs.project_id, "relPath": fs.rel_path }),
        );
    }
    if let Some(rel) = &out.notes_rel_path {
        emitter.emit_event("notes-changed", json!({ "relPath": rel }));
    }
    if let Some(project_id) = out.task_project_id {
        emitter.emit_event("tasks-changed", json!({ "projectId": project_id }));
    }
    if out.trash_id.is_some() {
        emitter.emit_event("trash-changed", json!({ "id": out.trash_id }));
    }
}
