// launchd (LaunchAgents) tool (Q.md computer-control slice): the agent
// installs, removes and inspects user LaunchAgents so a task survives the
// app closing. list/status are read-only; create/remove always confirm.
// Every launchctl call goes through argv (no shell interpolation); plists
// are XML-escaped, the label is validated reverse-DNS, and only the file
// ~/Library/LaunchAgents/<label>.plist named after the validated label is
// ever written or deleted. A failed load removes the plist it just wrote:
// no silent success.
use super::envelope_r;
use crate::agent::result::{io_fail, read_capped, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::path_policy::home_dir;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCHCTL_OUTPUT_CAP: usize = 20_000;

struct CmdOut {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_cmd(program: &str, args: &[&str]) -> Result<CmdOut> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(AppError::Io(format!("cannot run {program}: {e}"))),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, LAUNCHCTL_OUTPUT_CAP)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, LAUNCHCTL_OUTPUT_CAP)));
    let deadline = Instant::now() + LAUNCHCTL_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!(
                "{program} {} timed out after {} s and was killed",
                args.first().copied().unwrap_or(""),
                LAUNCHCTL_TIMEOUT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, _) = out_thread.join().unwrap_or_default().unwrap_or_default();
    let (stderr, _) = err_thread.join().unwrap_or_default().unwrap_or_default();
    Ok(CmdOut {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

fn gui_uid() -> Result<u32> {
    let out = run_cmd("id", &["-u"])?;
    if out.code != 0 {
        return Err(AppError::Io(format!("cannot get uid: {}", out.stderr.trim())));
    }
    out.stdout
        .trim()
        .parse::<u32>()
        .map_err(|_| AppError::Io("cannot parse uid from id -u".into()))
}

// Reverse-DNS label: at least two dot-separated components of ASCII letters,
// digits and inner hyphens; the label also names the plist file, so path
// separators and dots-only components can never reach the filesystem.
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 128
        && label.contains('.')
        && label.split('.').all(|c| {
            !c.is_empty()
                && !c.starts_with('-')
                && !c.ends_with('-')
                && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// Reads the first <string> value after <key>key</key> from a plist text
// (for ProgramArguments the first array element is the first <string>).
fn plist_string_after(text: &str, key: &str) -> Option<String> {
    let pat = format!("<key>{key}</key>");
    let idx = text.find(&pat)? + pat.len();
    let rest = &text[idx..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = start + rest[start..].find("</string>")?;
    Some(xml_unescape(&rest[start..end]))
}

#[derive(Debug, PartialEq)]
struct CalendarSpec {
    hour: u32,
    minute: u32,
    weekday: Option<u32>,
}

#[derive(Debug, PartialEq)]
struct JobSpec {
    label: String,
    program: String,
    args: Vec<String>,
    run_at_load: bool,
    start_interval: Option<u64>,
    calendar: Option<CalendarSpec>,
}

fn parse_create_args(args: &Value) -> std::result::Result<JobSpec, String> {
    let label = args.get("label").and_then(Value::as_str).unwrap_or("");
    if !valid_label(label) {
        return Err(
            "label must be reverse-DNS: at least two dot-separated components of letters, digits and inner hyphens (e.g. ru.example.backup)"
                .into(),
        );
    }
    let program = args.get("program").and_then(Value::as_str).unwrap_or("").trim();
    if program.is_empty() || !program.starts_with('/') {
        return Err("program must be an absolute path".into());
    }
    if fs::symlink_metadata(program).is_err() {
        return Err(format!("program \"{program}\" does not exist"));
    }
    let mut job_args: Vec<String> = Vec::new();
    match args.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for item in list {
                let text = item.as_str().ok_or_else(|| "args must be strings".to_string())?;
                job_args.push(text.to_string());
            }
        }
        Some(_) => return Err("args must be an array of strings".into()),
    }
    let run_at_load = args
        .get("run_at_load")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let start_interval = match args.get("start_interval_seconds") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| "start_interval_seconds must be an integer > 0".to_string())?,
        ),
    };
    let calendar = match args.get("calendar") {
        None | Some(Value::Null) => None,
        Some(obj) => {
            let hour = obj
                .get("hour")
                .and_then(Value::as_u64)
                .filter(|n| *n <= 23)
                .ok_or_else(|| "calendar.hour must be an integer 0..23".to_string())?;
            let minute = obj
                .get("minute")
                .and_then(Value::as_u64)
                .filter(|n| *n <= 59)
                .ok_or_else(|| "calendar.minute must be an integer 0..59".to_string())?;
            let weekday = match obj.get("weekday") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64()
                        .filter(|n| *n <= 7)
                        .ok_or_else(|| "calendar.weekday must be an integer 0..7".to_string())?,
                ),
            };
            Some(CalendarSpec {
                hour: hour as u32,
                minute: minute as u32,
                weekday: weekday.map(|w| w as u32),
            })
        }
    };
    if start_interval.is_some() && calendar.is_some() {
        return Err("use either start_interval_seconds or calendar, not both".into());
    }
    if start_interval.is_none() && calendar.is_none() && !run_at_load {
        return Err("create needs run_at_load=true, start_interval_seconds or calendar".into());
    }
    Ok(JobSpec {
        label: label.to_string(),
        program: program.to_string(),
        args: job_args,
        run_at_load,
        start_interval,
        calendar,
    })
}

fn build_plist(spec: &JobSpec) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    s += &format!(
        "\t<key>Label</key>\n\t<string>{}</string>\n",
        xml_escape(&spec.label)
    );
    s += "\t<key>ProgramArguments</key>\n\t<array>\n";
    s += &format!("\t\t<string>{}</string>\n", xml_escape(&spec.program));
    for a in &spec.args {
        s += &format!("\t\t<string>{}</string>\n", xml_escape(a));
    }
    s += "\t</array>\n";
    s += &format!(
        "\t<key>RunAtLoad</key>\n\t<{} />\n",
        if spec.run_at_load { "true" } else { "false" }
    );
    if let Some(si) = spec.start_interval {
        s += &format!("\t<key>StartInterval</key>\n\t<integer>{si}</integer>\n");
    }
    if let Some(c) = &spec.calendar {
        s += "\t<key>StartCalendarInterval</key>\n\t<dict>\n";
        s += &format!("\t\t<key>Hour</key>\n\t\t<integer>{}</integer>\n", c.hour);
        s += &format!("\t\t<key>Minute</key>\n\t\t<integer>{}</integer>\n", c.minute);
        if let Some(w) = c.weekday {
            s += &format!("\t\t<key>Weekday</key>\n\t\t<integer>{w}</integer>\n");
        }
        s += "\t</dict>\n";
    }
    s += "</dict>\n</plist>\n";
    s
}

fn launch_agents_dir() -> PathBuf {
    home_dir().join("Library/LaunchAgents")
}

// `launchctl list` output: header "PID\tStatus\tLabel", then one row per
// service; a "-" PID means loaded but not running.
fn parse_launchctl_list(stdout: &str) -> Vec<(Option<i64>, i64, String)> {
    let mut out = Vec::new();
    for line in stdout.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let pid = match cols.next() {
            None => continue,
            Some("-") => None,
            Some(p) => match p.parse::<i64>() {
                Ok(v) => Some(v),
                Err(_) => continue,
            },
        };
        let status = cols.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(-1);
        if let Some(label) = cols.next() {
            out.push((pid, status, label.to_string()));
        }
    }
    out
}

fn not_loaded(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("could not find service")
        || lower.contains("no such service")
        || lower.contains("no such process")
        || lower.contains("boot-out failed: 3")
}

pub struct LaunchdTool;

impl Tool for LaunchdTool {
    fn name(&self) -> &str {
        "launchd"
    }
    fn description(&self) -> &str {
        "Manage macOS LaunchAgents (~/Library/LaunchAgents) via launchctl so tasks survive the app closing. action=list (label/state/program), create (reverse-DNS label, absolute existing program, args, run_at_load, one of start_interval_seconds or calendar {hour,minute,weekday}), remove (only that label's plist), status (launchctl print). list/status read-only; create/remove confirm; a failed load removes the plist it wrote."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "create", "remove", "status"] },
                "label": { "type": "string" },
                "program": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "run_at_load": { "type": "boolean" },
                "start_interval_seconds": { "type": "integer" },
                "calendar": {
                    "type": "object",
                    "properties": {
                        "hour": { "type": "integer" },
                        "minute": { "type": "integer" },
                        "weekday": { "type": "integer" }
                    },
                    "required": ["hour", "minute"],
                    "additionalProperties": false
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        match args.get("action").and_then(Value::as_str) {
            Some("create") => Some(format!(
                "установка LaunchAgent {}: программа будет запускаться системой",
                args.get("label").and_then(Value::as_str).unwrap_or("?")
            )),
            Some("remove") => Some(format!(
                "удаление LaunchAgent {}",
                args.get("label").and_then(Value::as_str).unwrap_or("?")
            )),
            _ => None,
        }
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let r = match action {
            "list" => list_agents(),
            "create" => create_agent(args),
            "remove" => remove_agent(args),
            "status" => status_agent(args),
            other => Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("unknown action \"{other}\" (list|create|remove|status)"),
            )),
        };
        Ok(r?.capped(self.max_result_len()))
    }
}

fn label_arg_or_fail(args: &Value) -> std::result::Result<String, ToolResult> {
    let label = args.get("label").and_then(Value::as_str).unwrap_or("");
    if !valid_label(label) {
        return Err(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            "label must be reverse-DNS (e.g. ru.example.backup)",
        ));
    }
    Ok(label.to_string())
}

fn list_agents() -> Result<ToolResult> {
    let listed = run_cmd("launchctl", &["list"])?;
    if listed.code != 0 {
        return Ok(ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!("launchctl list failed (exit {}): {}", listed.code, listed.stderr.trim()),
        ));
    }
    let loaded = parse_launchctl_list(&listed.stdout);
    let dir = launch_agents_dir();
    let mut items: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                continue;
            }
            let Some(label) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let text = fs::read_to_string(&path).unwrap_or_default();
            let program = plist_string_after(&text, "Program")
                .or_else(|| plist_string_after(&text, "ProgramArguments"));
            let (pid, status) = loaded
                .iter()
                .find(|(_, _, l)| l == label)
                .map(|(pid, st, _)| (*pid, Some(*st)))
                .unwrap_or((None, None));
            let state = match (pid, status) {
                (Some(_), _) => "running",
                (None, Some(_)) => "loaded",
                _ => "unloaded",
            };
            seen.push(label.to_string());
            items.push(json!({
                "label": label,
                "state": state,
                "program": program,
                "pid": pid,
                "status": status,
                "plist": path.display().to_string(),
            }));
        }
    }
    for (pid, status, label) in loaded {
        if seen.contains(&label) {
            continue;
        }
        items.push(json!({
            "label": label,
            "state": if pid.is_some() { "running" } else { "loaded" },
            "program": Value::Null,
            "pid": pid,
            "status": status,
            "plist": Value::Null,
        }));
    }
    Ok(ToolResult::ok(json!({ "items": items, "count": items.len() })))
}

fn create_agent(args: &Value) -> Result<ToolResult> {
    let spec = match parse_create_args(args) {
        Ok(s) => s,
        Err(m) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, m)),
    };
    let dir = launch_agents_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        return Ok(io_fail(&dir, &e));
    }
    let plist_path = dir.join(format!("{}.plist", spec.label));
    if fs::symlink_metadata(&plist_path).is_ok() {
        return Ok(ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!(
                "{} already exists; remove it first or pick another label",
                plist_path.display()
            ),
        ));
    }
    if let Err(e) = fs::write(&plist_path, build_plist(&spec)) {
        return Ok(io_fail(&plist_path, &e));
    }
    let uid = match gui_uid() {
        Ok(u) => u,
        Err(e) => {
            let _ = fs::remove_file(&plist_path);
            return Ok(envelope_r(e));
        }
    };
    let target = format!("gui/{uid}");
    let plist_str = plist_path.display().to_string();
    let mut out = run_cmd("launchctl", &["bootstrap", &target, &plist_str])?;
    let mut method = "bootstrap";
    if out.code != 0 {
        let load = run_cmd("launchctl", &["load", "-w", &plist_str])?;
        if load.code != 0 {
            let _ = fs::remove_file(&plist_path);
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "cannot load LaunchAgent {}: bootstrap: {} | load: {}",
                    spec.label,
                    out.stderr.trim(),
                    load.stderr.trim()
                ),
            ));
        }
        out = load;
        method = "load";
    }
    Ok(ToolResult::ok(json!({
        "label": spec.label,
        "plist": plist_str,
        "loaded": true,
        "method": method,
        "warning": out.stderr.trim(),
    })))
}

fn remove_agent(args: &Value) -> Result<ToolResult> {
    let label = match label_arg_or_fail(args) {
        Ok(l) => l,
        Err(r) => return Ok(r),
    };
    let plist_path = launch_agents_dir().join(format!("{label}.plist"));
    let meta = match fs::symlink_metadata(&plist_path) {
        Ok(m) => m,
        Err(e) => return Ok(io_fail(&plist_path, &e)),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Ok(ToolResult::err(
            ToolErrorCode::PermissionDenied,
            format!("{} is not a regular file", plist_path.display()),
        ));
    }
    let uid = gui_uid()?;
    let service = format!("gui/{uid}/{label}");
    let boot = run_cmd("launchctl", &["bootout", &service])?;
    let was_loaded = boot.code == 0;
    if boot.code != 0 && !not_loaded(&boot.stderr) {
        let unload = run_cmd("launchctl", &["unload", &plist_path.display().to_string()])?;
        if unload.code != 0 && !not_loaded(&unload.stderr) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "cannot unload LaunchAgent {label}: bootout: {} | unload: {}",
                    boot.stderr.trim(),
                    unload.stderr.trim()
                ),
            ));
        }
    }
    if let Err(e) = fs::remove_file(&plist_path) {
        return Ok(io_fail(&plist_path, &e));
    }
    Ok(ToolResult::ok(json!({
        "label": label,
        "removed": true,
        "was_loaded": was_loaded,
    })))
}

fn status_agent(args: &Value) -> Result<ToolResult> {
    let label = match label_arg_or_fail(args) {
        Ok(l) => l,
        Err(r) => return Ok(r),
    };
    let uid = gui_uid()?;
    let out = run_cmd("launchctl", &["print", &format!("gui/{uid}/{label}")])?;
    if out.code != 0 {
        return Ok(ToolResult::err(
            ToolErrorCode::NotFound,
            format!(
                "launchctl print gui/{uid}/{label} failed: {}",
                out.stderr.trim()
            ),
        ));
    }
    Ok(ToolResult::ok(json!({ "label": label, "print": out.stdout })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use serde_json::json;

    #[test]
    fn label_validation_accepts_reverse_dns_and_rejects_the_rest() {
        assert!(valid_label("ru.example.backup"));
        assert!(valid_label("com.a1-b.c2"));
        assert!(!valid_label("backup"));
        assert!(!valid_label("ru..example"));
        assert!(!valid_label(".ru.example"));
        assert!(!valid_label("ru.example."));
        assert!(!valid_label("ru.-example.x"));
        assert!(!valid_label("ru.exa mple.x"));
        assert!(!valid_label("ru.example.x/../evil"));
        assert!(!valid_label(""));
    }

    #[test]
    fn plist_generation_escapes_values_and_emits_schedule() {
        let spec = JobSpec {
            label: "ru.example.backup".into(),
            program: "/usr/bin/backup & <run>".into(),
            args: vec!["--dest".into(), "/tmp/a b".into(), "<x>".into()],
            run_at_load: true,
            start_interval: None,
            calendar: Some(CalendarSpec {
                hour: 3,
                minute: 30,
                weekday: Some(1),
            }),
        };
        let plist = build_plist(&spec);
        assert!(plist.contains("<string>/usr/bin/backup &amp; &lt;run&gt;</string>"));
        assert!(plist.contains("<string>&lt;x&gt;</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true />"));
        assert!(plist.contains("<key>StartCalendarInterval</key>"));
        assert!(plist.contains("<key>Hour</key>\n\t\t<integer>3</integer>"));
        assert!(plist.contains("<key>Weekday</key>\n\t\t<integer>1</integer>"));
        assert!(!plist.contains("<key>StartInterval</key>"));
        assert!(plist.starts_with("<?xml version=\"1.0\""));
    }

    #[test]
    fn plist_generation_keeps_start_interval_and_omits_calendar() {
        let spec = JobSpec {
            label: "ru.example.tick".into(),
            program: "/bin/ls".into(),
            args: vec![],
            run_at_load: false,
            start_interval: Some(600),
            calendar: None,
        };
        let plist = build_plist(&spec);
        assert!(plist.contains("<key>StartInterval</key>\n\t<integer>600</integer>"));
        assert!(!plist.contains("StartCalendarInterval"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<false />"));
    }

    #[test]
    fn create_args_validation() {
        let ok = parse_create_args(&json!({
            "label": "ru.example.backup",
            "program": "/bin/ls",
            "args": ["-la"],
            "run_at_load": true
        }))
        .unwrap();
        assert_eq!(ok.args, vec!["-la"]);
        assert!(ok.run_at_load);

        assert!(parse_create_args(&json!({ "label": "single", "program": "/bin/ls", "run_at_load": true }))
            .unwrap_err()
            .contains("reverse-DNS"));
        assert!(parse_create_args(&json!({ "label": "ru.example.x", "program": "ls", "run_at_load": true }))
            .unwrap_err()
            .contains("absolute path"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x",
            "program": "/definitely/not/here-xyz",
            "run_at_load": true
        }))
        .unwrap_err()
        .contains("does not exist"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x",
            "program": "/bin/ls",
            "start_interval_seconds": 60,
            "calendar": { "hour": 3, "minute": 0 }
        }))
        .unwrap_err()
        .contains("not both"));
        assert!(parse_create_args(&json!({ "label": "ru.example.x", "program": "/bin/ls" }))
            .unwrap_err()
            .contains("needs run_at_load"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x", "program": "/bin/ls", "start_interval_seconds": 0
        }))
        .unwrap_err()
        .contains("> 0"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x", "program": "/bin/ls", "start_interval_seconds": -5
        }))
        .unwrap_err()
        .contains("> 0"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x", "program": "/bin/ls",
            "calendar": { "hour": 24, "minute": 0 }
        }))
        .unwrap_err()
        .contains("0..23"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x", "program": "/bin/ls",
            "calendar": { "hour": 3, "minute": 60 }
        }))
        .unwrap_err()
        .contains("0..59"));
        assert!(parse_create_args(&json!({
            "label": "ru.example.x", "program": "/bin/ls",
            "calendar": { "hour": 3, "minute": 0, "weekday": 8 }
        }))
        .unwrap_err()
        .contains("0..7"));
        let cal = parse_create_args(&json!({
            "label": "ru.example.x", "program": "/bin/ls",
            "calendar": { "hour": 3, "minute": 0 }
        }))
        .unwrap();
        assert_eq!(
            cal.calendar,
            Some(CalendarSpec {
                hour: 3,
                minute: 0,
                weekday: None
            })
        );
    }

    #[test]
    fn schema_rejects_unknown_and_wrong_typed_args() {
        let schema = LaunchdTool.parameters();
        assert!(validate_args(&schema, &json!({ "action": "list" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "explode" }))
            .unwrap_err()
            .contains("enum"));
        assert!(validate_args(&schema, &json!({ "action": "create", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
        assert!(validate_args(&schema, &json!({ "action": "create", "args": "not-an-array" }))
            .unwrap_err()
            .contains("array"));
        assert!(validate_args(
            &schema,
            &json!({ "action": "create", "calendar": { "hour": 3, "minute": 0, "extra": 1 } })
        )
        .unwrap_err()
        .contains("extra"));
    }

    #[test]
    fn plist_string_after_reads_program_and_arguments() {
        let text = build_plist(&JobSpec {
            label: "ru.example.x".into(),
            program: "/usr/bin/tool & co".into(),
            args: vec!["--flag".into()],
            run_at_load: true,
            start_interval: None,
            calendar: None,
        });
        assert_eq!(
            plist_string_after(&text, "ProgramArguments").as_deref(),
            Some("/usr/bin/tool & co")
        );
        assert_eq!(plist_string_after(&text, "Program"), None);
    }

    #[test]
    fn launchctl_list_rows_parse_with_dash_pid() {
        let rows = parse_launchctl_list("PID\tStatus\tLabel\n123\t0\tru.example.run\n-\t785\tru.example.idle\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (Some(123), 0, "ru.example.run".to_string()));
        assert_eq!(rows[1], (None, 785, "ru.example.idle".to_string()));
    }

    #[test]
    fn not_loaded_detection_covers_launchctl_phrases() {
        assert!(not_loaded("Could not find service \"gui/501/x\" in domain for user gui: 0x3"));
        assert!(not_loaded("Boot-out failed: 3: No such process"));
        assert!(!not_loaded("Permission denied"));
    }

    #[test]
    fn danger_reason_only_for_create_and_remove() {
        let ctx = ToolCtx::default();
        assert!(LaunchdTool
            .danger_reason(&json!({ "action": "list" }), &ctx)
            .is_none());
        assert!(LaunchdTool
            .danger_reason(&json!({ "action": "status", "label": "ru.example.x" }), &ctx)
            .is_none());
        assert!(LaunchdTool
            .danger_reason(&json!({ "action": "create", "label": "ru.example.x" }), &ctx)
            .is_some());
        assert!(LaunchdTool
            .danger_reason(&json!({ "action": "remove", "label": "ru.example.x" }), &ctx)
            .is_some());
    }
}
