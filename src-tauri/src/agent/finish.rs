// finish_task: the only way the loop ends normally (SPEC 6.4).
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx, ToolResult};
use crate::error::Result;
use serde_json::{json, Value};

pub struct FinishTaskTool;

impl Tool for FinishTaskTool {
    fn name(&self) -> &str {
        "finish_task"
    }
    fn description(&self) -> &str {
        "End this run. action=finish delivers `answer` to the user; action=stop aborts with `answer` as the reason."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["finish", "stop"] },
                "answer": { "type": "string" }
            },
            "required": ["action", "answer"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Meta
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        Ok(ToolResult::ok(json!({
            "action": args["action"],
            "answer": args["answer"],
        })))
    }
}
