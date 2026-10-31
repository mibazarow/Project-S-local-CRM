// fs_grep: read-only recursive case-insensitive text search (SPEC 5.2
// item 1). Skips binary, oversized files and .git/node_modules/target.
use crate::agent::project_tools::require_root;
use crate::agent::result::{io_fail, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::{lenient_existing, lenient_fail, resolve_path};
use crate::error::Result;
use crate::paths;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GREP_SCANNED_FILES: usize = 10_000;
const GREP_SKIP_DIRS: [&str; 3] = ["node_modules", "target", ".git"];

fn grep_file(path: &Path, lower: &str, matches: &mut Vec<Value>) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return;
    };
    for (no, line) in text.lines().enumerate() {
        if line.to_lowercase().contains(lower) {
            let text: String = line.chars().take(200).collect();
            matches.push(json!({
                "path": path.display().to_string(),
                "line": no + 1,
                "text": text,
            }));
        }
    }
}

pub struct FsGrepTool;

impl Tool for FsGrepTool {
    fn name(&self) -> &str {
        "fs_grep"
    }
    fn description(&self) -> &str {
        "Recursive case-insensitive text search under a folder (default: Harness/). Skips binary, >1 MiB files and .git/node_modules/target; returns path, line, text per match."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "max_results": { "type": "number" }
            },
            "required": ["pattern"],
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
        let pattern = args["pattern"].as_str().unwrap_or_default();
        if pattern.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "pattern is empty"));
        }
        let max = args
            .get("max_results")
            .and_then(Value::as_f64)
            .unwrap_or(50.0)
            .clamp(1.0, 200.0) as usize;
        let root = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => match lenient_existing(&resolve_path(p, &wd)) {
                Ok(p) => p,
                Err(e) => return Ok(lenient_fail(&e)),
            },
            _ if ctx.scope.is_some() => wd.clone(),
            _ => paths::harness_dir(&wd),
        };
        if let Err(e) = fs::metadata(&root) {
            return Ok(io_fail(&root, &e));
        }
        let lower = pattern.to_lowercase();
        let mut matches: Vec<Value> = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        let mut stack = vec![root.clone()];
        'walk: while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if meta.is_dir() {
                    if name.starts_with('.') || GREP_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(entry.path());
                } else {
                    scanned += 1;
                    if scanned > MAX_GREP_SCANNED_FILES {
                        truncated = true;
                        break 'walk;
                    }
                    if meta.len() > MAX_GREP_FILE_BYTES {
                        continue;
                    }
                    grep_file(&entry.path(), &lower, &mut matches);
                    if matches.len() >= max {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
        }
        matches.truncate(max);
        Ok(ToolResult::ok(json!({
            "pattern": pattern,
            "root": root.display().to_string(),
            "matches": matches,
            "truncated": truncated,
        }))
        .capped(self.max_result_len()))
    }
}
