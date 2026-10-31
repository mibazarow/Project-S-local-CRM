// fs_read: read-only file read (SPEC 5.2 item 1). Text is returned as-is,
// docx/xlsx/pdf are extracted to text, images and binaries are rejected.
use crate::agent::project_tools::require_root;
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{envelope_r, lenient_existing, lenient_fail, resolve_path, IMAGE_READ_HINT};
use crate::error::Result;
use crate::{extract, fs_ops};
use serde_json::{json, Value};
use std::fs;

const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;

// Envelope headroom for extracted documents: the JSON wrapper around the
// content (keys, path, escaping) costs a little, so the extracted text is
// capped below max_result_len to keep the capped envelope inside the limit.
pub(crate) const EXTRACT_ENVELOPE_RESERVE: usize = 512;

pub struct FsReadTool;

impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a file anywhere on the machine (absolute, ~/ or working-dir-relative path). Text files are returned as-is; docx/xlsx/xlsm/pdf are extracted to text (extracted: true). Images and other binary or >2 MiB files are rejected."
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
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_root(ctx)?;
        let path = resolve_path(args["path"].as_str().unwrap_or_default(), &wd);
        let path = match lenient_existing(&path) {
            Ok(p) => p,
            Err(e) => return Ok(lenient_fail(&e)),
        };
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        if meta.is_dir() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, format!("{} is a directory", path.display())));
        }
        if meta.len() > MAX_READ_BYTES {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("{} is {} bytes, over the {}-byte read cap; use fs_grep or a narrower read", path.display(), meta.len(), MAX_READ_BYTES),
            ));
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if fs_ops::IMAGE_EXTS.contains(&ext.as_str()) {
            return Ok(ToolResult::err(ToolErrorCode::ExecutionError, IMAGE_READ_HINT));
        }
        if matches!(extract::kind(&path), Some("docx") | Some("xlsx") | Some("pdf")) {
            let max_chars = self.max_result_len() - EXTRACT_ENVELOPE_RESERVE;
            return match extract::extract_text(&path, max_chars) {
                Ok(content) => Ok(ToolResult::ok(json!({
                    "path": path.display().to_string(),
                    "content": content,
                    "extracted": true,
                    "format": ext,
                }))
                .capped(self.max_result_len())),
                Err(e) => Ok(envelope_r(e)),
            };
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Ok(io_fail(&path, &e)),
        };
        if bytes.contains(&0) {
            return Ok(ToolResult::err(ToolErrorCode::ExecutionError, format!("{} looks binary", path.display())));
        }
        Ok(ToolResult::ok(json!({
            "path": path.display().to_string(),
            "content": String::from_utf8_lossy(&bytes),
        }))
        .capped(self.max_result_len()))
    }
}
