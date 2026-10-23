// fs_write: create or overwrite a text file (SPEC 5.2 item 2). Writes inside
// Harness/ and Projects/ skip confirmation; the previous content is versioned
// so the write is undoable.
use crate::agent::project_tools::{emit_fs_changed_for_abs, emit_versions_changed, require_wd, with_conn};
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{in_default_territory, lenient_existing, lenient_fail, lenient_target_parent, resolve_path};
use crate::error::Result;
use serde_json::{json, Value};
use std::fs;

pub struct FsWriteTool;

impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a text file anywhere on the machine; parent folders are created. Writes inside Harness/ and Projects/ run without confirmation, elsewhere the user confirms."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
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
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = resolve_path(raw, wd);
        (!in_default_territory(&path, wd)).then(|| "запись вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let content = args["content"].as_str().unwrap_or_default();
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        // An existing file (also via a lenient variant match) is overwritten;
        // a new one keeps the requested name under leniently resolved parents.
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) if e.to_string().contains("ambiguous") => return Ok(lenient_fail(&e)),
            Err(_) => lenient_target_parent(&path)?,
        };
        if let Ok(meta) = fs::metadata(&path) {
            if meta.is_dir() {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, format!("{} is a directory", path.display())));
            }
        }
        let existed = path.exists();
        let previous = if existed {
            fs::read_to_string(&path).ok()
        } else {
            None
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return Ok(io_fail(parent, &e));
            }
        }
        match fs::write(&path, content) {
            Ok(()) => {
                let abs = path.display().to_string();
                emit_fs_changed_for_abs(ctx, &path);
                if let Some(prev) = previous {
                    let version_id = with_conn(ctx, |conn| {
                        crate::reliability::version_save(conn, &wd, "harness", &abs, &prev)
                    })
                    .ok();
                    if version_id.is_some() {
                        emit_versions_changed(ctx, "harness", &abs);
                        let _ = with_conn(ctx, |conn| {
                            crate::undo::push_file_write(
                                conn,
                                "harness",
                                &abs,
                                version_id,
                                false,
                                None,
                                None,
                            )
                        });
                    }
                } else if !existed {
                    let _ = with_conn(ctx, |conn| {
                        crate::undo::push_file_write(
                            conn,
                            "harness",
                            &abs,
                            None,
                            true,
                            None,
                            None,
                        )
                    });
                }
                Ok(ToolResult::ok(json!({
                    "path": path.display().to_string(),
                    "bytesWritten": content.len(),
                })))
            }
            Err(e) => Ok(io_fail(&path, &e)),
        }
    }
}
