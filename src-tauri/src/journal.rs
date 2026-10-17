// Run/tool journal (SPEC 3.3 "Settings/logs — журнал запусков и tool-calls").
// The feature is switched off (JOURNAL_ENABLED = false): log_event is a no-op
// and retention pruning is skipped. mask_secrets stays in use by the
// git/ssh/bash tools.
use crate::error::Result;
use chrono::{Local, NaiveDate};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PREVIEW_CHARS: usize = 240;
const FILE_PREFIX: &str = "runs-";
const FILE_SUFFIX: &str = ".jsonl";

// Master switch for the run journal: when false, nothing is ever written to
// Settings/logs and prune never deletes anything.
const JOURNAL_ENABLED: bool = false;

// Which run a journal line belongs to: project chat, harness session, or a
// schedule firing (SPEC 5.3).
#[derive(Debug, Clone, Copy)]
pub struct RunInfo {
    pub kind: &'static str,
    pub chat_id: i64,
    pub project_id: Option<i64>,
    pub session_id: Option<i64>,
}

struct Inner {
    dir: PathBuf,
    // (local date of the open file, writer); rotated when the date changes.
    current: Mutex<Option<(String, BufWriter<fs::File>)>>,
}

// Cheap to clone: every run thread shares one writer state.
#[derive(Clone)]
pub struct Journal {
    inner: Arc<Inner>,
}

impl Journal {
    pub fn open(dir: &Path) -> Result<Journal> {
        fs::create_dir_all(dir)?;
        Ok(Journal {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                current: Mutex::new(None),
            }),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_event(
        &self,
        run: &RunInfo,
        step: usize,
        tool: &str,
        ok: bool,
        code: Option<&str>,
        duration_ms: u128,
        args: &Value,
        result: &Value,
    ) -> Result<()> {
        if !JOURNAL_ENABLED {
            return Ok(());
        }
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut record = json!({
            "ts": Local::now().to_rfc3339(),
            "kind": run.kind,
            "chatId": run.chat_id,
            "step": step,
            "tool": tool,
            "ok": ok,
            "ms": duration_ms,
            "args": preview(args),
            "result": preview(result),
        });
        let obj = record.as_object_mut().expect("json! object");
        if let Some(id) = run.project_id {
            obj.insert("projectId".into(), json!(id));
        }
        if let Some(id) = run.session_id {
            obj.insert("sessionId".into(), json!(id));
        }
        if let Some(code) = code {
            obj.insert("code".into(), json!(code));
        }
        let line = format!("{record}\n");

        let mut guard = self.inner.current.lock().unwrap();
        let needs_open = match guard.as_ref() {
            Some((date, _)) => *date != today,
            None => true,
        };
        if needs_open {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.inner.dir.join(format!("{FILE_PREFIX}{today}{FILE_SUFFIX}")))?;
            if let Some((_, previous)) = guard.replace((today, BufWriter::new(file))) {
                let _ = previous.into_inner(); // flush the previous day's file
            }
        }
        let (_, writer) = guard.as_mut().expect("writer just opened");
        writer.write_all(line.as_bytes())?;
        writer.flush()?;
        Ok(())
    }

    // Deletes run-log files whose filename date is older than `days` days
    // before `today`; 0 disables pruning. Returns the number of files
    // removed. `today` is injectable so retention is testable.
    pub fn prune(&self, today: NaiveDate, days: u32) -> Result<usize> {
        if !JOURNAL_ENABLED || days == 0 {
            return Ok(0);
        }
        let cutoff = today - chrono::Duration::days(i64::from(days));
        let mut removed = 0;
        for entry in fs::read_dir(&self.inner.dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let Some(date_part) = name
                .strip_prefix(FILE_PREFIX)
                .and_then(|n| n.strip_suffix(FILE_SUFFIX))
            else {
                continue;
            };
            let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") else {
                continue;
            };
            if date < cutoff {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            // Drop the cached writer so a pruned file is never resurrected
            // by a stale buffer.
            *self.inner.current.lock().unwrap() = None;
        }
        Ok(removed)
    }

    pub fn prune_old(&self, days: u32) -> Result<usize> {
        self.prune(Local::now().date_naive(), days)
    }
}

// Short, masked preview of tool args/results for one journal line.
fn preview(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let clipped: String = text.chars().take(PREVIEW_CHARS).collect();
    mask_secrets(&clipped)
}

// Key names whose value gets masked when it follows `:` or `=`.
const SECRET_KEY_SUFFIXES: [&str; 6] = [
    "key",
    "token",
    "secret",
    "password",
    "authorization",
    "credential",
];

// Dependency-free masking heuristic (SPEC 7 privacy):
// 1. values of `name: value` / `name=value` pairs whose name ends with a
//    secret hint are replaced with `***`;
// 2. bare `sk-…` style API keys are masked wherever they appear.
// Full file contents never reach the journal anyway (preview cap above).
pub fn mask_secrets(text: &str) -> String {
    mask_bare_api_keys(&mask_key_value_pairs(text))
}

fn mask_key_value_pairs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b':' || byte == b'=' {
            // Key = alphanumeric run right before the separator, skipping
            // whitespace and an optional closing quote.
            let mut k = i;
            while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
                k -= 1;
            }
            if k > 0 && bytes[k - 1] == b'"' {
                k -= 1;
            }
            let mut start = k;
            while start > 0
                && (bytes[start - 1] == b'_'
                    || bytes[start - 1] == b'-'
                    || (bytes[start - 1] as char).is_ascii_alphanumeric())
            {
                start -= 1;
            }
            let key = String::from_utf8_lossy(&bytes[start..k]).to_lowercase();
            if SECRET_KEY_SUFFIXES.iter().any(|suffix| key.ends_with(suffix)) {
                out.push(byte as char);
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                    out.push(' ');
                    j += 1;
                }
                let quoted = j < bytes.len() && bytes[j] == b'"';
                if quoted {
                    out.push('"');
                    j += 1;
                }
                out.push_str("***");
                if quoted {
                    while j < bytes.len() && bytes[j] != b'"' {
                        j += 1;
                    }
                    if j < bytes.len() {
                        out.push('"');
                        j += 1;
                    }
                } else {
                    // Unquoted value: skip to the next delimiter so the
                    // masked value is not re-emitted after `***`.
                    while j < bytes.len() {
                        let c = bytes[j] as char;
                        if c.is_whitespace() || c == ',' || c == '}' || c == '"' {
                            break;
                        }
                        j += 1;
                    }
                }
                i = j;
                continue;
            }
        }
        let ch = text[i..].chars().next().expect("char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn mask_bare_api_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("sk-") {
        out.push_str(&rest[..pos]);
        out.push_str("sk-***");
        let mut tail = &rest[pos + 3..];
        while let Some(ch) = tail.chars().next() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                tail = &tail[ch.len_utf8()..];
            } else {
                break;
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use std::fs;

    fn run_info() -> RunInfo {
        RunInfo {
            kind: "project",
            chat_id: 7,
            project_id: Some(3),
            session_id: None,
        }
    }

    #[test]
    fn log_event_is_a_noop_when_journal_disabled() {
        let dir = temp_test_dir("journal-disabled-lines");
        let journal = Journal::open(&dir).unwrap();
        journal
            .log_event(
                &run_info(),
                2,
                "fs_write_text",
                false,
                Some("denied_by_user"),
                12,
                &json!({ "rel_path": "a.md", "apiKey": "sk-abc123def456", "content": "x".repeat(500) }),
                &json!({ "ok": false, "error": { "code": "denied_by_user" } }),
            )
            .unwrap();
        journal
            .log_event(&run_info(), 3, "finish_task", true, None, 1, &json!({}), &json!({"ok": true}))
            .unwrap();
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "no run-log files are written while the journal is disabled"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mask_covers_key_value_pairs_and_bare_api_keys() {
        assert_eq!(
            mask_secrets(r#"{"apiKey":"hunter2","path":"a.md"}"#),
            r#"{"apiKey":"***","path":"a.md"}"#
        );
        assert_eq!(mask_secrets("api_key=sk-abc123def456"), "api_key=***");
        assert_eq!(mask_secrets("token: my-secret-token"), "token: ***");
        assert_eq!(mask_secrets("header sk-live-AbC099_xy end"), "header sk-*** end");
        // Non-secret keys pass through untouched.
        assert_eq!(mask_secrets(r#"{"rel_path":"a.md","limit":10}"#), r#"{"rel_path":"a.md","limit":10}"#);
    }

    #[test]
    fn prune_is_skipped_when_journal_disabled() {
        let dir = temp_test_dir("journal-disabled-prune");
        let journal = Journal::open(&dir).unwrap();
        for date in ["2026-09-01", "2026-10-03", "2026-10-15"] {
            fs::write(dir.join(format!("runs-{date}.jsonl")), "{}\n").unwrap();
        }
        fs::write(dir.join("other.log"), "keep").unwrap();

        let today = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        assert_eq!(journal.prune(today, 14).unwrap(), 0, "pruning is skipped while disabled");
        assert!(dir.join("runs-2026-09-01.jsonl").is_file());
        assert!(dir.join("runs-2026-10-03.jsonl").is_file());
        assert!(dir.join("runs-2026-10-15.jsonl").is_file());
        assert!(dir.join("other.log").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }
}
