// fs_copy/fs_move tool wrappers (SPEC 5.2 item 2): the shared body lives in
// transfer.rs; here only the schema and the CheckConditional policy.
// Confirmation is skipped only when both endpoints are inside Harness/ and
// Projects/ and nothing is replaced; overwrite always confirms, even inside
// the territory.
use crate::agent::harness_tools::transfer::fs_transfer;
use crate::agent::result::ToolResult;
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{in_default_territory, resolve_path};
use crate::error::Result;
use serde_json::{json, Value};

fn transfer_danger_reason(args: &Value, ctx: &ToolCtx, verb: &str) -> Option<String> {
    let wd = ctx.scope.as_deref().or(ctx.working_dir.as_deref())?;
    let from = resolve_path(args.get("from").and_then(Value::as_str)?, wd);
    let to = resolve_path(args.get("to").and_then(Value::as_str)?, wd);
    let mut reasons: Vec<String> = Vec::new();
    if !(in_default_territory(&from, wd, ctx.scope.is_some()) && in_default_territory(&to, wd, ctx.scope.is_some())) {
        reasons.push(format!("{verb} вне Harness/ и Projects/"));
    }
    if args.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
        reasons.push("overwrite=true перезапишет существующий файл".to_string());
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

pub struct FsCopyTool;

impl Tool for FsCopyTool {
    fn name(&self) -> &str {
        "fs_copy"
    }
    fn description(&self) -> &str {
        "Copy a file or folder (recursively) anywhere on the machine (absolute, ~/ or working-dir-relative paths). Parent folders of the target are created; to may be a new path or an existing folder (the source is placed into it by name). Symlinks are never followed or copied, and a folder is never copied into itself. The destination is only replaced with overwrite=true, which replaces files but never merges or replaces folders. Copies inside Harness/ and Projects/ run without confirmation unless overwrite=true; elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string" },
                "to": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        transfer_danger_reason(args, ctx, "копирование")
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        fs_transfer(args, ctx, false, self.max_result_len())
    }
}

pub struct FsMoveTool;

impl Tool for FsMoveTool {
    fn name(&self) -> &str {
        "fs_move"
    }
    fn description(&self) -> &str {
        "Move a file or folder anywhere on the machine (rename; falls back to copy+delete across volumes). to may be a new path or an existing folder (the source is moved into it by name); parent folders are created. Symlinks are never followed, and a folder is never moved into itself. The destination is only replaced with overwrite=true, which replaces files but never folders. Moves inside Harness/ and Projects/ run without confirmation unless overwrite=true; elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string" },
                "to": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        transfer_danger_reason(args, ctx, "перемещение")
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        fs_transfer(args, ctx, true, self.max_result_len())
    }
}
