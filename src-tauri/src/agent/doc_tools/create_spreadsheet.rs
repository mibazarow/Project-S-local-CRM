// create_spreadsheet: writes XLSX/CSV from typed rows with native Rust
// writers. The path extension picks the format; project mode keeps every path
// inside the project root (Skip), harness mode accepts absolute/~/working-dir-
// relative paths and skips confirmation only inside Harness/ and Projects/
// (CheckConditional). Existing files are only replaced with overwrite=true.
use super::paths::{
    content_cap_error, extension, guard_target, resolve_target, write_atomic, MAX_CONTENT_BYTES,
};
use super::spreadsheet::{build_csv, build_xlsx, SHEET_FORMATS};
use super::super::harness_tools::{in_default_territory, resolve_path};
use super::super::project_tools::{envelope, require_root};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use serde_json::{json, Value};

pub struct CreateSpreadsheetTool {
    harness: bool,
}

impl CreateSpreadsheetTool {
    pub fn project() -> Self {
        CreateSpreadsheetTool { harness: false }
    }
    pub fn harness() -> Self {
        CreateSpreadsheetTool { harness: true }
    }
}

impl Tool for CreateSpreadsheetTool {
    fn name(&self) -> &str {
        "create_spreadsheet"
    }
    fn description(&self) -> &str {
        "Create a spreadsheet with native Rust writers (no external tools). The path extension picks the format: .xlsx (one sheet per entry, first row bold, auto column widths, numbers stored as numbers) or .csv (exactly one sheet, RFC-style quoting). Cell values are string|number|boolean|null. Existing files are only replaced with overwrite=true. path is project-relative (project agent) or absolute/~/working-dir-relative (harness agent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "sheets": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "rows": {
                                "type": "array",
                                "items": {
                                    "type": "array",
                                    "items": { "type": ["string", "number", "boolean", "null"] }
                                }
                            }
                        },
                        "required": ["name", "rows"],
                        "additionalProperties": false
                    }
                },
                "overwrite": { "type": "boolean" }
            },
            "required": ["path", "sheets"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        if self.harness {
            PermissionPolicy::CheckConditional
        } else {
            PermissionPolicy::Skip
        }
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        if !self.harness {
            return None;
        }
        let wd = ctx.scope.as_deref().or(ctx.working_dir.as_deref())?;
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = resolve_path(raw, wd);
        (!in_default_territory(&path, wd, ctx.scope.is_some()))
            .then(|| "создание файла вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_root(ctx)?;
        let (label, path) = match resolve_target(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let format = extension(&path);
        if !SHEET_FORMATS.contains(&format.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "path must end with .xlsx or .csv",
            ));
        }
        let sheets = match args["sheets"].as_array() {
            Some(list) if !list.is_empty() => list.clone(),
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "sheets must be a non-empty array",
                ))
            }
        };
        let json_len = serde_json::to_string(&sheets).map(|s| s.len()).unwrap_or(usize::MAX);
        if json_len > MAX_CONTENT_BYTES {
            return Ok(content_cap_error(json_len));
        }
        if let Err(r) = guard_target(&path, args) {
            return Ok(r);
        }
        let bytes = if format == "csv" {
            if sheets.len() != 1 {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "CSV supports exactly one sheet",
                ));
            }
            build_csv(&sheets[0])
        } else {
            build_xlsx(&sheets)?
        };
        match write_atomic(&path, &bytes) {
            Ok(written) => Ok(ToolResult::ok(json!({
                "path": label,
                "format": format,
                "bytes": written,
            }))),
            Err(e) => Ok(envelope::<()>(Err(e))),
        }
    }
}
