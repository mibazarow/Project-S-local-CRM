// Spotlight (mdfind) search tool (Q.md computer-control slice): read-only
// whole-disk search by name/content/kind, far faster than walking folders.
// The query is passed to mdfind as a single argv element (no shell), output
// is NUL-separated (-0) so paths with newlines survive, only_in is resolved
// and must be an existing folder, and the limit is clamped to 1..500.
use crate::agent::project_tools::require_wd;
use crate::agent::result::{read_capped, read_capped_bytes, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::path_policy::resolve_path;
use chrono::SecondsFormat;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MDFIND_TIMEOUT: Duration = Duration::from_secs(20);
const MDFIND_OUTPUT_CAP: usize = 200_000;
const LIMIT_DEFAULT: u64 = 50;
const LIMIT_MAX: u64 = 500;

// Escapes a user value embedded into a quoted Spotlight query fragment.
fn quote_fragment(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// Builds the mdfind argv: options first, then one combined query string.
// Returns (argv, joined query) or a validation message.
fn build_mdfind_args(
    query: &str,
    name: &str,
    kind: &str,
    only_in: Option<&PathBuf>,
) -> std::result::Result<(Vec<String>, String), String> {
    let mut parts: Vec<String> = Vec::new();
    if !query.trim().is_empty() {
        parts.push(query.trim().to_string());
    }
    if !name.trim().is_empty() {
        parts.push(format!(
            "kMDItemDisplayName == \"*{}*\"c",
            quote_fragment(name.trim())
        ));
    }
    if !kind.trim().is_empty() {
        parts.push(format!(
            "kMDItemKind == \"{}\"",
            quote_fragment(kind.trim())
        ));
    }
    if parts.is_empty() {
        return Err("query, name or kind is required".into());
    }
    let joined = if parts.len() == 1 {
        parts[0].clone()
    } else {
        parts
            .iter()
            .map(|p| format!("({p})"))
            .collect::<Vec<_>>()
            .join(" && ")
    };
    let mut argv = vec!["-0".to_string()];
    if let Some(dir) = only_in {
        argv.push("-onlyin".into());
        argv.push(dir.display().to_string());
    }
    argv.push(joined.clone());
    Ok((argv, joined))
}

fn clamp_limit(value: Option<u64>) -> u64 {
    value.unwrap_or(LIMIT_DEFAULT).clamp(1, LIMIT_MAX)
}

// Splits NUL-separated mdfind output into at most `limit` paths.
fn parse_nul_paths(bytes: &[u8], limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in bytes.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        out.push(String::from_utf8_lossy(chunk).into_owned());
        if out.len() >= limit {
            break;
        }
    }
    out
}

pub struct SpotlightSearchTool;

impl Tool for SpotlightSearchTool {
    fn name(&self) -> &str {
        "mdfind"
    }
    fn description(&self) -> &str {
        "Read-only whole-disk Spotlight search (mdfind): query is a raw Spotlight query (kMDItemTextContent…), name matches a file-name substring, kind matches the Finder kind (PDF, folder); combined with AND. only_in limits to an existing folder; limit default 50, max 500. Returns path/name/is_dir/size/modified."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "name": { "type": "string" },
                "kind": { "type": "string" },
                "only_in": { "type": "string" },
                "limit": { "type": "integer" }
            },
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
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        let only_in = match args.get("only_in").and_then(Value::as_str) {
            None | Some("") => None,
            Some(raw) => {
                let dir = resolve_path(raw, &wd);
                if !dir.is_dir() {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        format!("only_in folder not found: {}", dir.display()),
                    ));
                }
                Some(dir)
            }
        };
        let limit = clamp_limit(args.get("limit").and_then(Value::as_u64));
        let (argv, joined) = match build_mdfind_args(query, name, kind, only_in.as_ref()) {
            Ok(v) => v,
            Err(m) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, m)),
        };

        let mut child = match Command::new("mdfind").args(&argv).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("cannot run mdfind: {e}"),
                ))
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped_bytes(r, MDFIND_OUTPUT_CAP)));
        let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, 4_000)));
        let deadline = Instant::now() + MDFIND_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("mdfind timed out after {} s and was killed", MDFIND_TIMEOUT.as_secs()),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let (out_bytes, _) = out_thread.join().unwrap_or_default().unwrap_or_default();
        let (err_text, _) = err_thread.join().unwrap_or_default().unwrap_or_default();
        if status.code().unwrap_or(-1) != 0 {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("mdfind failed (exit {}): {}", status.code().unwrap_or(-1), err_text.trim()),
            ));
        }

        let paths = parse_nul_paths(&out_bytes, limit as usize);
        let mut items: Vec<Value> = Vec::new();
        for p in paths {
            let path = PathBuf::from(&p);
            let meta = fs::symlink_metadata(&path);
            let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(p.clone());
            let mut item = json!({ "path": p, "name": name });
            if let Ok(m) = meta {
                item["is_dir"] = json!(m.is_dir());
                if m.is_file() {
                    item["size"] = json!(m.len());
                }
                if let Some(mtime) = m
                    .modified()
                    .ok()
                    .and_then(|t| chrono::DateTime::<chrono::Utc>::try_from(t).ok())
                {
                    item["modified"] = json!(mtime.to_rfc3339_opts(SecondsFormat::Secs, true));
                }
            }
            items.push(item);
        }
        Ok(ToolResult::ok(json!({ "query": joined, "count": items.len(), "items": items }))
            .capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_single_query_and_nul_separation() {
        let (argv, joined) = build_mdfind_args("kMDItemTextContent == \"invoice\"c", "", "", None).unwrap();
        assert_eq!(argv[0], "-0");
        assert_eq!(argv.len(), 2);
        assert_eq!(joined, "kMDItemTextContent == \"invoice\"c");
    }

    #[test]
    fn combines_name_kind_and_query_with_and() {
        let (argv, joined) = build_mdfind_args("hello", "report", "PDF", None).unwrap();
        assert_eq!(
            joined,
            "(hello) && (kMDItemDisplayName == \"*report*\"c) && (kMDItemKind == \"PDF\")"
        );
        assert_eq!(argv.last().unwrap(), &joined);
    }

    #[test]
    fn only_in_becomes_onlyin_option() {
        let dir = PathBuf::from("/Users/tester/Documents");
        let (argv, _) = build_mdfind_args("", "x", "", Some(&dir)).unwrap();
        assert_eq!(argv, vec!["-0", "-onlyin", "/Users/tester/Documents", "kMDItemDisplayName == \"*x*\"c"]);
    }

    #[test]
    fn rejects_empty_query_set() {
        let err = build_mdfind_args("", "  ", "", None).unwrap_err();
        assert!(err.contains("required"));
    }

    #[test]
    fn escapes_quotes_in_generated_fragments() {
        let (_, joined) = build_mdfind_args("", "a\"b", "", None).unwrap();
        assert!(joined.contains("*a\\\"b*"), "{joined}");
    }

    #[test]
    fn parses_nul_separated_paths_with_limit() {
        let paths = parse_nul_paths(b"/a/one\0/b/two\0/c/three\0", 10);
        assert_eq!(paths, vec!["/a/one", "/b/two", "/c/three"]);
        let limited = parse_nul_paths(b"/a\0/b\0/c\0", 2);
        assert_eq!(limited, vec!["/a", "/b"]);
        assert!(parse_nul_paths(b"", 10).is_empty());
        assert_eq!(parse_nul_paths(b"/x\0\0/y\0", 10), vec!["/x", "/y"]);
    }

    #[test]
    fn limit_clamps_to_default_and_max() {
        assert_eq!(clamp_limit(None), 50);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(1000)), 500);
        assert_eq!(clamp_limit(Some(7)), 7);
    }

    #[test]
    fn policy_is_read_only_skip() {
        assert_eq!(SpotlightSearchTool.class(), ToolClass::ReadOnly);
        assert_eq!(SpotlightSearchTool.policy(), PermissionPolicy::Skip);
    }
}
