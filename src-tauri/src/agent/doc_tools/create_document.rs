// create_document: turns basic Markdown into DOCX/PDF/MD/TXT/HTML via the
// native writers. Project mode keeps every path inside the project root (Skip,
// like write_file); harness mode accepts absolute/~/working-dir-relative paths
// and skips confirmation only inside Harness/ and Projects/ (CheckConditional,
// like fs_write). Existing files are only replaced with an explicit
// overwrite=true; content is capped at ~2 MiB.
use super::docx::build_docx;
use super::markdown::parse_markdown;
use super::paths::{
    content_cap_error, extension, guard_target, resolve_target, write_atomic, MAX_CONTENT_BYTES,
};
use super::pdf::build_pdf;
use super::super::harness_tools::{in_default_territory, resolve_path};
use super::super::project_tools::{envelope, require_wd};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::text_html::{html_wrapper, plain_with_title};
use crate::error::Result;
use serde_json::{json, Value};

pub struct CreateDocumentTool {
    harness: bool,
}

impl CreateDocumentTool {
    pub fn project() -> Self {
        CreateDocumentTool { harness: false }
    }
    pub fn harness() -> Self {
        CreateDocumentTool { harness: true }
    }
}

impl Tool for CreateDocumentTool {
    fn name(&self) -> &str {
        "create_document"
    }
    fn description(&self) -> &str {
        "Create a document from Markdown with native Rust writers (no external tools). format: docx|pdf|md|txt|html, and the path extension must match it. Markdown supports headings (#..######), paragraphs, bullet (-, *) and numbered lists, fenced code blocks and pipe tables. DOCX gets a title, sized headings, indented lists, monospace code and bordered tables; PDF uses a Cyrillic-capable macOS system font with word wrap and multi-page pagination; md/txt write the Markdown as-is; html wraps it in a minimal escaped page. Existing files are only replaced with overwrite=true. path is project-relative (project agent) or absolute/~/working-dir-relative (harness agent)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "format": { "type": "string", "enum": ["docx", "pdf", "md", "txt", "html"] },
                "title": { "type": "string" },
                "markdown": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["path", "format", "markdown"],
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
        let wd = ctx.working_dir.as_deref()?;
        let raw = args.get("path").and_then(Value::as_str)?;
        let path = resolve_path(raw, wd);
        (!in_default_territory(&path, wd))
            .then(|| "создание файла вне Harness/ и Projects/".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let format = args["format"].as_str().unwrap_or_default();
        let markdown = args["markdown"].as_str().unwrap_or_default();
        let title = args.get("title").and_then(Value::as_str);
        if markdown.len() > MAX_CONTENT_BYTES {
            return Ok(content_cap_error(markdown.len()));
        }
        let (label, path) = match resolve_target(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let ext = extension(&path);
        if ext != format {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("path extension \"{ext}\" does not match format \"{format}\""),
            ));
        }
        if let Err(r) = guard_target(&path, args) {
            return Ok(r);
        }
        let blocks = parse_markdown(markdown);
        let bytes = match format {
            "docx" => build_docx(title, &blocks)?,
            "pdf" => build_pdf(title, &blocks)?,
            "md" => plain_with_title(title, markdown, true).into_bytes(),
            "txt" => plain_with_title(title, markdown, false).into_bytes(),
            "html" => html_wrapper(title, markdown).into_bytes(),
            other => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("unsupported format \"{other}\""),
                ))
            }
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
