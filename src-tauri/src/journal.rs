// Run/tool journal (SPEC 3.3 "Settings/logs — журнал запусков и tool-calls",
// 6 "все вызовы пишутся в журнал запусков"): one JSONL file per local day,
// one line per tool call. Writes are buffered behind a Mutex and flushed per
// line, so logging never blocks a run meaningfully and a crash loses at most
// the in-flight line. Previews are short and secret-masked: the journal must
// never carry full file contents or credentials.
use crate::error::Result;
use crate::paths;
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PREVIEW_CHARS: usize = 240;
const FILE_PREFIX: &str = "runs-";
const FILE_SUFFIX: &str = ".jsonl";

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
        if days == 0 {
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

// ---------- Reading (the "Журнал" section, SPEC 3.3) ----------

pub const DEFAULT_DAYS: u32 = 7;
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;

// One parsed journal line. A line missing any required field fails to
// deserialize and is skipped, so malformed lines never reach the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub ts: String,
    pub kind: String,
    pub chat_id: i64,
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default)]
    pub session_id: Option<i64>,
    pub step: usize,
    pub tool: String,
    pub ok: bool,
    #[serde(default)]
    pub code: Option<String>,
    pub ms: u64,
    pub args: String,
    pub result: String,
}

#[derive(Debug, Clone)]
pub struct JournalFilter {
    pub days: u32,
    pub kind: Option<String>,
    pub tool: Option<String>,
    pub ok: Option<bool>,
    pub project_id: Option<i64>,
    pub session_id: Option<i64>,
    pub limit: Option<usize>,
}

impl Default for JournalFilter {
    fn default() -> Self {
        JournalFilter {
            days: DEFAULT_DAYS,
            kind: None,
            tool: None,
            ok: None,
            project_id: None,
            session_id: None,
            limit: None,
        }
    }
}

// One journal_list payload: filtered entries plus the day/tool options the
// UI filter bar offers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalListResult {
    pub entries: Vec<JournalEntry>,
    pub days: Vec<String>,
    pub tools: Vec<String>,
}
// Date from a `runs-YYYY-MM-DD.jsonl` file name.
fn file_date(name: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(
        name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?,
        "%Y-%m-%d",
    )
    .ok()
}

fn ts_key(entry: &JournalEntry) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(&entry.ts).ok()
}

// All entries from files whose filename date is within the last `days` days,
// sorted newest first (unparseable ts last), plus the distinct tool names
// seen in the scanned range. Malformed lines are skipped, never fatal.
fn scan_range(wd: &Path, days: u32) -> Result<(Vec<JournalEntry>, Vec<String>)> {
    let cutoff = Local::now().date_naive() - chrono::Duration::days(i64::from(days));
    let mut entries: Vec<JournalEntry> = Vec::new();
    let mut tools: BTreeSet<String> = BTreeSet::new();
    if let Ok(read_dir) = fs::read_dir(paths::logs_dir(wd)) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let Some(date) = file_date(&name) else {
                continue;
            };
            if date < cutoff {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let Ok(parsed) = serde_json::from_str::<JournalEntry>(line) else {
                    continue;
                };
                tools.insert(parsed.tool.clone());
                entries.push(parsed);
            }
        }
    }
    entries.sort_by(|a, b| match (ts_key(a), ts_key(b)) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => b.ts.cmp(&a.ts),
    });
    Ok((entries, tools.into_iter().collect()))
}

fn matches(entry: &JournalEntry, filter: &JournalFilter) -> bool {
    filter.kind.as_ref().map_or(true, |k| &entry.kind == k)
        && filter.tool.as_ref().map_or(true, |t| &entry.tool == t)
        && filter.ok.map_or(true, |ok| entry.ok == ok)
        && filter.project_id.map_or(true, |id| entry.project_id == Some(id))
        && filter.session_id.map_or(true, |id| entry.session_id == Some(id))
}

// Filtered entries (limit applied after filtering and sorting), newest first.
pub fn list_entries(wd: &Path, filter: &JournalFilter) -> Result<Vec<JournalEntry>> {
    let (entries, _) = scan_range(wd, filter.days)?;
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    Ok(entries
        .into_iter()
        .filter(|e| matches(e, filter))
        .take(limit)
        .collect())
}

// Distinct tool names seen in the scanned range (UI tool filter options).
pub fn journal_tools(wd: &Path, days: u32) -> Result<Vec<String>> {
    Ok(scan_range(wd, days)?.1)
}

// Dates that have run-log files, newest first (UI filter hints).
pub fn journal_days(wd: &Path) -> Result<Vec<String>> {
    let mut dates: Vec<String> = Vec::new();
    if let Ok(read_dir) = fs::read_dir(paths::logs_dir(wd)) {
        for entry in read_dir.flatten() {
            let Some(name) = entry.path().file_name().map(|n| n.to_string_lossy().to_string())
            else {
                continue;
            };
            if let Some(date) = file_date(&name) {
                dates.push(date.format("%Y-%m-%d").to_string());
            }
        }
    }
    dates.sort();
    dates.dedup();
    dates.reverse();
    Ok(dates)
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
    fn log_event_writes_one_masked_jsonl_line_per_tool_call() {
        let dir = temp_test_dir("journal-lines");
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

        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with("runs-") && files[0].ends_with(".jsonl"));

        let text = fs::read_to_string(dir.join(&files[0])).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        let first = &lines[0];
        assert_eq!(first["kind"], "project");
        assert_eq!(first["chatId"], 7);
        assert_eq!(first["projectId"], 3);
        assert!(first.get("sessionId").is_none());
        assert_eq!(first["step"], 2);
        assert_eq!(first["tool"], "fs_write_text");
        assert_eq!(first["ok"], false);
        assert_eq!(first["code"], "denied_by_user");
        assert!(first["ms"].as_u64().unwrap() < 10_000);
        let args = first["args"].as_str().unwrap();
        assert!(!args.contains("sk-abc123def456"), "api key leaked: {args}");
        assert!(args.contains("***"));
        assert!(args.chars().count() <= PREVIEW_CHARS);
        assert_eq!(lines[1].get("code"), None);
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
    fn prune_removes_only_files_older_than_retention_by_filename_date() {
        let dir = temp_test_dir("journal-prune");
        let journal = Journal::open(&dir).unwrap();
        for date in ["2026-09-01", "2026-10-03", "2026-10-15"] {
            fs::write(dir.join(format!("runs-{date}.jsonl")), "{}\n").unwrap();
        }
        fs::write(dir.join("other.log"), "keep").unwrap();

        let today = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        assert_eq!(journal.prune(today, 0).unwrap(), 0); // 0 disables pruning
        assert_eq!(journal.prune(today, 14).unwrap(), 1); // cutoff 2026-10-02
        assert!(!dir.join("runs-2026-09-01.jsonl").exists());
        assert!(dir.join("runs-2026-10-03.jsonl").is_file());
        assert!(dir.join("runs-2026-10-15.jsonl").is_file());
        assert!(dir.join("other.log").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_does_not_resurrect_a_deleted_current_file() {
        let dir = temp_test_dir("journal-prune-current");
        let journal = Journal::open(&dir).unwrap();
        journal
            .log_event(&run_info(), 1, "echo", true, None, 1, &json!({}), &json!({"ok": true}))
            .unwrap();
        // Prune the current file away (injected future date), then log
        // again: the file comes back with only the new line, not the stale
        // buffered one.
        let future = Local::now().date_naive() + chrono::Duration::days(2);
        assert_eq!(journal.prune(future, 1).unwrap(), 1);
        journal
            .log_event(&run_info(), 2, "echo", true, None, 1, &json!({}), &json!({"ok": true}))
            .unwrap();
        let text = fs::read_dir(&dir)
            .unwrap()
            .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
            .collect::<String>();
        assert_eq!(text.lines().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn line(
        ts: &str,
        kind: &str,
        tool: &str,
        ok: bool,
        project: Option<i64>,
        session: Option<i64>,
        code: Option<&str>,
    ) -> String {
        let mut record = json!({
            "ts": ts, "kind": kind, "chatId": 7, "step": 1,
            "tool": tool, "ok": ok, "ms": 5, "args": "a", "result": "r",
        });
        let obj = record.as_object_mut().unwrap();
        if let Some(id) = project {
            obj.insert("projectId".into(), json!(id));
        }
        if let Some(id) = session {
            obj.insert("sessionId".into(), json!(id));
        }
        if let Some(code) = code {
            obj.insert("code".into(), json!(code));
        }
        record.to_string()
    }

    // Three log files: today (two entries + malformed lines), yesterday, and
    // one 30 days back (outside the default 7-day range).
    fn seed_journal(wd: &Path) -> (NaiveDate, NaiveDate, NaiveDate) {
        let logs = paths::logs_dir(wd);
        fs::create_dir_all(&logs).unwrap();
        let today = Local::now().date_naive();
        let yday = today - chrono::Duration::days(1);
        let old = today - chrono::Duration::days(30);
        fs::write(
            logs.join(format!("runs-{today}.jsonl")),
            format!(
                "{}\nnot-json at all\n{}\n{{\"ts\": \"broken\n",
                line(
                    &format!("{today}T10:00:00+00:00"),
                    "project",
                    "fs_write_text",
                    true,
                    Some(3),
                    None,
                    None,
                ),
                line(
                    &format!("{today}T12:00:00+00:00"),
                    "harness",
                    "echo",
                    false,
                    None,
                    Some(9),
                    Some("boom"),
                ),
            ),
        )
        .unwrap();
        fs::write(
            logs.join(format!("runs-{yday}.jsonl")),
            line(
                &format!("{yday}T23:00:00+00:00"),
                "schedule",
                "echo",
                true,
                None,
                None,
                None,
            ),
        )
        .unwrap();
        fs::write(
            logs.join(format!("runs-{old}.jsonl")),
            line(
                &format!("{old}T09:00:00+00:00"),
                "project",
                "old_tool",
                true,
                Some(3),
                None,
                None,
            ),
        )
        .unwrap();
        (today, yday, old)
    }

    #[test]
    fn list_entries_parses_sorts_newest_first_and_skips_malformed() {
        let wd = temp_test_dir("journal-list");
        let (today, yday, old) = seed_journal(&wd);

        let entries = list_entries(&wd, &JournalFilter::default()).unwrap();
        assert_eq!(
            entries.len(),
            3,
            "malformed lines skipped, file older than the range excluded"
        );
        assert_eq!(entries[0].ts, format!("{today}T12:00:00+00:00"));
        assert_eq!(entries[0].kind, "harness");
        assert_eq!(entries[0].code.as_deref(), Some("boom"));
        assert_eq!(entries[0].session_id, Some(9));
        assert_eq!(entries[0].project_id, None);
        assert_eq!(entries[1].ts, format!("{today}T10:00:00+00:00"));
        assert_eq!(entries[1].project_id, Some(3));
        assert_eq!(entries[2].ts, format!("{yday}T23:00:00+00:00"));
        assert_eq!(entries[2].kind, "schedule");
        assert_eq!(entries[2].chat_id, 7);
        assert_eq!(entries[2].step, 1);
        assert_eq!(entries[2].ms, 5);
        assert_eq!(entries[2].args, "a");
        assert_eq!(entries[2].result, "r");
        assert!(list_entries(&wd, &JournalFilter { days: 60, ..Default::default() })
            .unwrap()
            .iter()
            .any(|e| e.ts.starts_with(&old.to_string())));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn list_entries_applies_each_filter_and_limit() {
        let wd = temp_test_dir("journal-filters");
        seed_journal(&wd);

        let by_tool = list_entries(&wd, &JournalFilter { tool: Some("echo".into()), ..Default::default() }).unwrap();
        assert_eq!(by_tool.len(), 2);
        let by_kind = list_entries(&wd, &JournalFilter { kind: Some("project".into()), ..Default::default() }).unwrap();
        assert_eq!(by_kind.len(), 1);
        assert_eq!(by_kind[0].tool, "fs_write_text");
        let only_errors = list_entries(&wd, &JournalFilter { ok: Some(false), ..Default::default() }).unwrap();
        assert_eq!(only_errors.len(), 1);
        assert_eq!(only_errors[0].code.as_deref(), Some("boom"));
        let only_ok = list_entries(&wd, &JournalFilter { ok: Some(true), ..Default::default() }).unwrap();
        assert_eq!(only_ok.len(), 2);
        let by_session = list_entries(&wd, &JournalFilter { session_id: Some(9), ..Default::default() }).unwrap();
        assert_eq!(by_session.len(), 1);
        assert_eq!(by_session[0].tool, "echo");
        let no_session = list_entries(&wd, &JournalFilter { session_id: Some(42), ..Default::default() }).unwrap();
        assert!(no_session.is_empty());
        let by_project = list_entries(&wd, &JournalFilter { project_id: Some(3), ..Default::default() }).unwrap();
        assert_eq!(by_project.len(), 1);
        let limited = list_entries(&wd, &JournalFilter { limit: Some(1), ..Default::default() }).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].tool, "echo", "limit keeps the newest entries");

        // The tool options for the UI filter bar cover the scanned range.
        assert_eq!(
            journal_tools(&wd, DEFAULT_DAYS).unwrap(),
            vec!["echo".to_string(), "fs_write_text".to_string()]
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn journal_days_lists_dates_newest_first() {
        let wd = temp_test_dir("journal-days");
        let (today, yday, old) = seed_journal(&wd);
        assert_eq!(
            journal_days(&wd).unwrap(),
            vec![today.to_string(), yday.to_string(), old.to_string()]
        );
        fs::remove_dir_all(&wd).unwrap();
    }
}
