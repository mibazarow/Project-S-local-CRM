// fs_mkdir: create a single folder (SPEC 5.2 item 2). The parent must already
// exist and the path must not; a near-duplicate name variant reports the real
// folder instead of creating a second one next to it.
use crate::agent::project_tools::require_wd;
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{in_default_territory, lenient_existing, lenient_fail, lenient_target_parent, resolve_path};
use crate::error::Result;
use serde_json::{json, Value};
use std::fs;

pub struct FsMkdirTool;

impl Tool for FsMkdirTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Create a single folder: the parent must already exist (parents are not created) and the path must not exist yet. Creation inside Harness/ and Projects/ runs without confirmation, elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
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
        (!in_default_territory(&path, wd)).then(|| "создание папки вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let raw = args["path"].as_str().unwrap_or_default();
        if raw.trim().is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "path is empty"));
        }
        let path = resolve_path(raw, &wd);
        // A variant of an existing folder name reports the real one instead
        // of creating a near-duplicate folder next to it.
        let path = match lenient_existing(&path) {
            Ok(existing) => {
                let kind = if fs::symlink_metadata(&existing)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
                {
                    "folder"
                } else {
                    "file"
                };
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} already exists as a {kind}", existing.display()),
                ));
            }
            Err(e) if e.to_string().contains("ambiguous") => return Ok(lenient_fail(&e)),
            Err(_) => lenient_target_parent(&path)?,
        };
        let Some(parent) = path.parent() else {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("{} has no parent folder", path.display()),
            ));
        };
        match fs::create_dir(&path) {
            Ok(()) => Ok(ToolResult::ok(json!({
                "path": path.display().to_string(),
                "created": true,
            }))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("parent folder does not exist: {}", parent.display()),
            )),
            Err(e) => Ok(io_fail(&path, &e)),
        }
    }
}
