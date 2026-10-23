// fs_list: read-only folder listing (SPEC 5.2 item 1): name, kind, size per
// entry, defaulting to the Harness/ territory.
use crate::agent::project_tools::require_wd;
use crate::agent::result::{io_fail, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{lenient_existing, lenient_fail, resolve_path};
use crate::error::Result;
use crate::paths;
use serde_json::{json, Value};
use std::fs;

const LIST_MAX_ENTRIES: usize = 500;

pub struct FsListTool;

impl Tool for FsListTool {
    fn name(&self) -> &str {
        "fs_list"
    }
    fn description(&self) -> &str {
        "List a folder anywhere on the machine (default: the Harness/ territory): name, kind, size per entry."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
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
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => match lenient_existing(&resolve_path(p, &wd)) {
                Ok(p) => p,
                Err(e) => return Ok(lenient_fail(&e)),
            },
            _ => paths::harness_dir(&wd),
        };
        let rd = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        let mut entries: Vec<Value> = rd
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some(json!({
                    "name": e.file_name().to_string_lossy(),
                    "kind": if meta.is_dir() { "dir" } else { "file" },
                    "size": if meta.is_file() { serde_json::json!(meta.len()) } else { Value::Null },
                }))
            })
            .collect();
        entries.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
        let truncated = entries.len() > LIST_MAX_ENTRIES;
        entries.truncate(LIST_MAX_ENTRIES);
        Ok(ToolResult::ok(json!({ "path": path.display().to_string(), "entries": entries, "truncated": truncated }))
            .capped(self.max_result_len()))
    }
}
