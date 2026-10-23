// fs_delete: reversible delete (SPEC 5.2 item 2). Always confirmed; the item
// moves into Settings/.trash and can be restored through the trash commands.
use crate::agent::project_tools::{emit_trash_changed, require_wd, with_conn};
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{canonicalish, envelope_r, lenient_existing, lenient_fail, normalize, resolve_path};
use crate::error::Result;
use crate::paths;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

// Minimal sanity guard for fs_delete; the confirmation remains the real one.
fn is_obviously_destructive(path: &Path, wd: &Path) -> bool {
    let resolved = canonicalish(path);
    let base = wd.canonicalize().unwrap_or_else(|_| normalize(wd));
    resolved == Path::new("/")
        || resolved == normalize(&crate::path_policy::home_dir())
        || resolved == base
        || resolved == base.join(paths::HARNESS_DIR)
        || resolved == base.join(paths::PROJECTS_DIR)
}

pub struct FsDeleteTool;

impl Tool for FsDeleteTool {
    fn name(&self) -> &str {
        "fs_delete"
    }
    fn description(&self) -> &str {
        "Delete a file or folder (recursively) anywhere on the machine. Dangerous: always confirmed."
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
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление файла или папки".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) => return Ok(lenient_fail(&e)),
        };
        if is_obviously_destructive(&path, &wd) {
            return Ok(ToolResult::err(
                ToolErrorCode::PermissionDenied,
                format!("refusing to delete a root folder: {}", path.display()),
            ));
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        // Reversible delete: the item moves into Settings/.trash and can be
        // restored through the trash commands.
        let kind = if meta.is_dir() { "dir" } else { "file" };
        let r = with_conn(ctx, |conn| {
            crate::reliability::trash_put(conn, &wd, "harness", kind, &path, None)
        });
        match r {
            Ok(id) => {
                let _ = with_conn(ctx, |conn| {
                    crate::undo::push_delete(conn, id, "harness", &path.display().to_string(), None, None)
                });
                emit_trash_changed(ctx, Some(id));
                Ok(ToolResult::ok(json!({
                    "deleted": path.display().to_string(),
                    "trashId": id,
                })))
            }
            Err(e) => Ok(envelope_r(e)),
        }
    }
}
