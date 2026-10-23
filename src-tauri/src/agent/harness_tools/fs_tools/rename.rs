// fs_rename: rename inside the current parent folder (SPEC 5.2 item 2).
// path plus a validated newName; the destination must not exist.
use crate::agent::project_tools::{require_wd, with_conn};
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{in_default_territory, lenient_existing, lenient_fail, resolve_path};
use crate::error::Result;
use crate::{fs_ops, undo};
use serde_json::{json, Value};
use std::fs;

pub struct FsRenameTool;

impl Tool for FsRenameTool {
    fn name(&self) -> &str {
        "fs_rename"
    }
    fn description(&self) -> &str {
        "Rename a file or folder inside its current parent folder: path plus newName (a plain name without separators or traversal). The destination must not exist. Renames inside Harness/ and Projects/ run without confirmation, elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "newName": { "type": "string" }
            },
            "required": ["path", "newName"],
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
        let wd = ctx.working_dir.as_deref()?;
        let path = resolve_path(args.get("path").and_then(Value::as_str)?, wd);
        (!in_default_territory(&path, wd)).then(|| "переименование вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) => return Ok(lenient_fail(&e)),
        };
        let new_name = match fs_ops::check_name(args["newName"].as_str().unwrap_or_default()) {
            Ok(name) => name,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string())),
        };
        let parent = match path.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} has no parent folder to rename in", path.display()),
                ))
            }
        };
        let target = parent.join(&new_name);
        if target != path && fs::symlink_metadata(&target).is_ok() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("destination exists: {}", target.display()),
            ));
        }
        match fs::rename(&path, &target) {
            Ok(()) => {
                let _ = with_conn(ctx, |conn| {
                    undo::push_rename(
                        conn,
                        "harness",
                        &path.display().to_string(),
                        &target.display().to_string(),
                        None,
                        None,
                    )
                });
                Ok(ToolResult::ok(json!({
                    "from": path.display().to_string(),
                    "to": target.display().to_string(),
                })))
            }
            Err(e) => Ok(io_fail(&path, &e)),
        }
    }
}
