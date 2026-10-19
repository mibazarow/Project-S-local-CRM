use super::super::macos_ui_tool::{as_quote, run_osascript};
use crate::agent::project_tools::require_wd;
use crate::agent::result::{read_capped, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::path_policy::resolve_path;
use chrono::SecondsFormat;
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use sysinfo::{Disks, Networks, System};

const OSASCRIPT_BINARY: &str = "/usr/bin/osascript";
const OPEN_BINARY: &str = "/usr/bin/open";
const PMSET_BINARY: &str = "/usr/bin/pmset";
const ROUTE_BINARY: &str = "/usr/bin/route";
const CMD_TIMEOUT: Duration = Duration::from_secs(5);
const CPU_SAMPLE: Duration = Duration::from_millis(250);
const MAX_INTERFACES: usize = 32;
const WATCH_DEFAULT_SECS: u64 = 10;
const WATCH_MAX_SECS: u64 = 60;
const MAX_WATCH_EVENTS: usize = 500;

struct CmdOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cmd(binary: &Path, args: &[String], timeout: Duration) -> std::result::Result<CmdOutput, String> {
    let mut child = match Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("cannot run {}: {e}", binary.display())),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, 64_000).0));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, 8_192).0));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}: {e}", binary.display()));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} timed out after {} s and was killed",
                binary.display(),
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(CmdOutput {
        code: status.code(),
        stdout: out_thread.join().unwrap_or_default().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default().unwrap_or_default(),
    })
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn battery_percent(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'%' {
            continue;
        }
        let mut j = i;
        while j > 0 && bytes[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < i {
            if let Ok(v) = text[j..i].parse::<u8>() {
                return Some(v);
            }
        }
    }
    None
}

fn battery_state(text: &str) -> Option<&'static str> {
    let low = text.to_ascii_lowercase();
    if low.contains("discharging") {
        Some("discharging")
    } else if low.contains("finishing charge") {
        Some("finishing")
    } else if low.contains("charging") {
        Some("charging")
    } else if low.contains("charged") {
        Some("charged")
    } else {
        None
    }
}

fn battery_info() -> Option<Value> {
    let out = run_cmd(Path::new(PMSET_BINARY), &["-g".into(), "batt".into()], CMD_TIMEOUT).ok()?;
    if out.code != Some(0) {
        return None;
    }
    let percent = battery_percent(&out.stdout)?;
    let state = battery_state(&out.stdout);
    Some(json!({
        "percent": percent,
        "state": state,
        "charging": match state {
            Some("charging") | Some("finishing") => Some(true),
            Some("discharging") | Some("charged") => Some(false),
            _ => None,
        },
    }))
}

fn parse_route_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(key))
        .map(|value| value.trim().to_owned())
}

fn default_route() -> Option<Value> {
    let out = run_cmd(
        Path::new(ROUTE_BINARY),
        &["-n".into(), "get".into(), "default".into()],
        CMD_TIMEOUT,
    )
    .ok()?;
    if out.code != Some(0) {
        return None;
    }
    let gateway = parse_route_field(&out.stdout, "gateway:");
    let interface = parse_route_field(&out.stdout, "interface:");
    if gateway.is_none() && interface.is_none() {
        return None;
    }
    Some(json!({ "gateway": gateway, "interface": interface }))
}

fn working_disk(wd: &Path) -> Option<Value> {
    let disks = Disks::new_with_refreshed_list();
    let canon = fs::canonicalize(wd).unwrap_or_else(|_| wd.to_path_buf());
    let best = disks
        .list()
        .iter()
        .filter(|d| canon.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().components().count())
        .or_else(|| disks.list().iter().find(|d| d.mount_point() == Path::new("/")))
        .or_else(|| disks.list().first())?;
    let total = best.total_space();
    let free = best.available_space();
    Some(json!({
        "mount": best.mount_point().to_string_lossy(),
        "name": best.name().to_string_lossy(),
        "file_system": best.file_system().to_string_lossy(),
        "total": total,
        "used": total.saturating_sub(free),
        "free": free,
    }))
}

fn network_info() -> Value {
    let nets = Networks::new_with_refreshed_list();
    let mut names: Vec<&String> = nets.list().keys().collect();
    names.sort();
    let interfaces: Vec<Value> = names
        .iter()
        .take(MAX_INTERFACES)
        .map(|name| {
            let net = &nets.list()[*name];
            let ips: Vec<String> = net
                .ip_networks()
                .iter()
                .map(|ip| format!("{}/{}", ip.addr, ip.prefix))
                .collect();
            let mac = if net.mac_address().is_unspecified() {
                Value::Null
            } else {
                json!(net.mac_address().to_string())
            };
            json!({ "name": name.as_str(), "ips": ips, "mac": mac })
        })
        .collect();
    json!({ "interfaces": interfaces, "default_route": default_route() })
}

pub struct SysinfoTool;

impl Tool for SysinfoTool {
    fn name(&self) -> &str {
        "sysinfo"
    }
    fn description(&self) -> &str {
        "Read-only system snapshot as one JSON object: cpu, memory, disk (working volume), battery, uptime_secs, network.\ncpu: usage_percent, load one/five/fifteen, cores_physical/cores_logical; memory: total/used/free/available bytes; disk: mount/name/file_system/total/used/free of the volume holding the working folder; battery: percent/state/charging, null on desktops; network: interfaces with ips and mac, default_route with gateway and interface. Best-effort: unavailable fields are null, never an error."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
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
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().unwrap_or_else(|| PathBuf::from("/"));
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        std::thread::sleep(CPU_SAMPLE);
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let load = System::load_average();
        Ok(ToolResult::ok(json!({
            "cpu": {
                "usage_percent": round1(sys.global_cpu_usage() as f64),
                "load": {
                    "one": round1(load.one),
                    "five": round1(load.five),
                    "fifteen": round1(load.fifteen),
                },
                "cores_physical": System::physical_core_count(),
                "cores_logical": sys.cpus().len(),
            },
            "memory": {
                "total": sys.total_memory(),
                "used": sys.used_memory(),
                "free": sys.free_memory(),
                "available": sys.available_memory(),
            },
            "disk": working_disk(&wd),
            "battery": battery_info(),
            "uptime_secs": System::uptime(),
            "network": network_info(),
        }))
        .capped(self.max_result_len()))
    }
}

pub struct NotifyTool {
    binary: PathBuf,
    timeout: Duration,
}

impl Default for NotifyTool {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(OSASCRIPT_BINARY),
            timeout: Duration::from_secs(10),
        }
    }
}

impl NotifyTool {
    #[cfg(test)]
    fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary,
            timeout: Duration::from_secs(10),
        }
    }
}

fn build_notify_script(args: &Value) -> std::result::Result<String, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .ok_or("title is required")?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or("message is required")?;
    if title.trim().is_empty() {
        return Err("title must not be empty".into());
    }
    if message.trim().is_empty() {
        return Err("message must not be empty".into());
    }
    let mut script = format!(
        "display notification {} with title {}",
        as_quote(message),
        as_quote(title)
    );
    if let Some(subtitle) = args.get("subtitle").and_then(Value::as_str) {
        if !subtitle.trim().is_empty() {
            script.push_str(&format!(" with subtitle {}", as_quote(subtitle)));
        }
    }
    if let Some(sound) = args.get("sound").and_then(Value::as_str) {
        if !sound.trim().is_empty() {
            script.push_str(&format!(" with sound name {}", as_quote(sound)));
        }
    }
    Ok(script)
}

impl Tool for NotifyTool {
    fn name(&self) -> &str {
        "notify"
    }
    fn description(&self) -> &str {
        "Show a macOS Notification Center banner via osascript (title and message required).\nOptional subtitle and sound (a system sound name like Glass or Basso). Non-destructive; returns {ok:true} or a clear error."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "message": { "type": "string" },
                "subtitle": { "type": "string" },
                "sound": { "type": "string" }
            },
            "required": ["title", "message"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let script = match build_notify_script(args) {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        if !self.binary.is_file() {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                format!(
                    "{} not found; osascript ships with every macOS install",
                    self.binary.display()
                ),
            ));
        }
        let (code, _stdout, stderr) = match run_osascript(&self.binary, &script, self.timeout) {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        if code != Some(0) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "osascript failed (exit {}): {}",
                    code.unwrap_or(-1),
                    stderr.trim()
                ),
            ));
        }
        Ok(ToolResult::ok(json!({
            "ok": true,
            "title": args["title"],
            "message": args["message"],
        })))
    }
}

pub struct OpenTool {
    binary: PathBuf,
    timeout: Duration,
}

impl Default for OpenTool {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(OPEN_BINARY),
            timeout: Duration::from_secs(15),
        }
    }
}

impl OpenTool {
    #[cfg(test)]
    fn with_binary(binary: PathBuf) -> Self {
        Self {
            binary,
            timeout: Duration::from_secs(15),
        }
    }
}

fn resolve_target(raw: &str, wd: &Path) -> std::result::Result<String, (ToolErrorCode, String)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err((ToolErrorCode::InvalidArgs, "target must not be empty".into()));
    }
    if let Some((scheme, rest)) = raw.split_once("://") {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" => Ok(raw.to_owned()),
            "file" => {
                let path = PathBuf::from(rest);
                if fs::symlink_metadata(&path).is_ok() {
                    Ok(path.display().to_string())
                } else {
                    Err((ToolErrorCode::NotFound, format!("file not found: {rest}")))
                }
            }
            other => Err((
                ToolErrorCode::InvalidArgs,
                format!("unsupported URL scheme \"{other}\": use http, https or file"),
            )),
        };
    }
    let path = resolve_path(raw, wd);
    if fs::symlink_metadata(&path).is_ok() {
        Ok(path.display().to_string())
    } else {
        Err((
            ToolErrorCode::NotFound,
            format!("target not found: {}", path.display()),
        ))
    }
}

impl Tool for OpenTool {
    fn name(&self) -> &str {
        "open"
    }
    fn description(&self) -> &str {
        "Open a path, URL or app in the macOS default handler (open command).\ntarget is an existing file/folder path (absolute, ~ or relative to the working dir) or an http(s)/file URL; optional app (application name or .app path) opens the target with that specific app. Non-destructive; returns {ok, target, app}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": { "type": "string" },
                "app": { "type": "string" }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args.get("target").and_then(Value::as_str).unwrap_or(""), &wd) {
            Ok(t) => t,
            Err((code, msg)) => return Ok(ToolResult::err(code, msg)),
        };
        let app = args
            .get("app")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty());
        let mut argv: Vec<String> = Vec::new();
        if let Some(app) = app {
            argv.push("-a".into());
            argv.push(app.to_owned());
        }
        argv.push(target.clone());
        if !self.binary.is_file() {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                format!(
                    "{} not found; the open command ships with every macOS install",
                    self.binary.display()
                ),
            ));
        }
        let out = match run_cmd(&self.binary, &argv, self.timeout) {
            Ok(o) => o,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        if out.code != Some(0) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "open failed (exit {}): {}",
                    out.code.unwrap_or(-1),
                    out.stderr.trim()
                ),
            ));
        }
        Ok(ToolResult::ok(json!({ "ok": true, "target": target, "app": app })))
    }
}

pub struct FsWatchTool;

fn watch_secs(value: Option<u64>) -> u64 {
    value.unwrap_or(WATCH_DEFAULT_SECS).clamp(1, WATCH_MAX_SECS)
}

fn event_kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Any => "any",
        EventKind::Other => "other",
        EventKind::Access(_) => "accessed",
        EventKind::Create(_) => "created",
        EventKind::Modify(_) => "modified",
        EventKind::Remove(_) => "removed",
    }
}

impl Tool for FsWatchTool {
    fn name(&self) -> &str {
        "fs_watch"
    }
    fn description(&self) -> &str {
        "Watch a folder for filesystem changes for up to timeout_secs seconds (default 10, max 60).\nReturns the observed events as {path, kind, at} with kind one of created/modified/removed/accessed/other/any. Blocks only for the given timeout; a quiet folder returns an empty events list with timed_out:true."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "timeout_secs": { "type": "integer" }
            },
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
        let wd = require_wd(ctx)?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        if raw.trim().is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "path is required"));
        }
        let dir = resolve_path(raw, &wd);
        if !dir.is_dir() {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                format!("folder not found: {}", dir.display()),
            ));
        }
        let secs = watch_secs(args.get("timeout_secs").and_then(Value::as_u64));
        let (tx, rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("cannot start watcher: {e}"),
                ))
            }
        };
        if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("cannot watch {}: {e}", dir.display()),
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut events: Vec<Value> = Vec::new();
        let mut watch_error: Option<String> = None;
        let timed_out = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.as_millis() == 0 {
                break true;
            }
            match rx.recv_timeout(remaining) {
                Ok(Ok(ev)) => {
                    let kind = event_kind_name(ev.kind);
                    let at = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                    for p in ev.paths {
                        events.push(json!({ "path": p.to_string_lossy(), "kind": kind, "at": at }));
                        if events.len() >= MAX_WATCH_EVENTS {
                            break;
                        }
                    }
                    if events.len() >= MAX_WATCH_EVENTS {
                        break false;
                    }
                }
                Ok(Err(e)) => {
                    watch_error = Some(format!("watcher error: {e}"));
                    break false;
                }
                Err(RecvTimeoutError::Timeout) => break true,
                Err(RecvTimeoutError::Disconnected) => {
                    watch_error = Some("watcher stopped unexpectedly".into());
                    break false;
                }
            }
        };
        drop(watcher);
        let mut data = json!({
            "path": dir.display().to_string(),
            "duration_secs": secs,
            "timed_out": timed_out,
            "count": events.len(),
            "events": events,
        });
        if let Some(e) = watch_error {
            data["watch_error"] = json!(e);
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use std::os::unix::fs::PermissionsExt;

    fn fake_bin(dir: &Path, name: &str, body: &str) -> PathBuf {
        let script = dir.join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn ctx_with_wd(wd: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(wd.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn sysinfo_returns_object_with_expected_keys() {
        let r = SysinfoTool.execute(&json!({}), &ToolCtx::default()).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        for key in ["cpu", "memory", "disk", "battery", "uptime_secs", "network"] {
            assert!(data.get(key).is_some(), "missing {key}");
        }
        assert!(data["cpu"]["cores_logical"].as_u64().unwrap() >= 1);
        assert!(data["cpu"]["usage_percent"].is_number());
        assert!(data["memory"]["total"].as_u64().unwrap() > 0);
        assert!(data["uptime_secs"].as_u64().unwrap() > 0);
        assert!(data["network"]["interfaces"].is_array());
    }

    #[test]
    fn working_disk_resolves_the_root_volume() {
        let d = working_disk(Path::new("/")).unwrap();
        assert_eq!(d["mount"], "/");
        assert!(d["total"].as_u64().unwrap() > 0);
        assert!(d["free"].as_u64().unwrap() > 0);
        assert!(d["used"].as_u64().unwrap() > 0);
    }

    #[test]
    fn battery_percent_and_state_parse_pmset_output() {
        let battery = "Now drawing from 'Battery Power'\n -InternalBattery-0\t95%; discharging; 4:21 remaining present: true\n";
        assert_eq!(battery_percent(battery), Some(95));
        assert_eq!(battery_state(battery), Some("discharging"));
        let charged = "Now drawing from 'AC Power'\n -InternalBattery-0\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(battery_percent(charged), Some(100));
        assert_eq!(battery_state(charged), Some("charged"));
        let charging = "Now drawing from 'AC Power'\n -InternalBattery-0\t42%; charging; 2:00 remaining present: true\n";
        assert_eq!(battery_percent(charging), Some(42));
        assert_eq!(battery_state(charging), Some("charging"));
        let desktop = "Now drawing from 'AC Power'\n";
        assert_eq!(battery_percent(desktop), None);
        assert_eq!(battery_state(desktop), None);
    }

    #[test]
    fn route_output_fields_are_parsed() {
        let out = "route to: default\ndestination: default\n       mask: default\n    gateway: 192.0.2.1\n  interface: en0\n";
        assert_eq!(parse_route_field(out, "gateway:").as_deref(), Some("192.0.2.1"));
        assert_eq!(parse_route_field(out, "interface:").as_deref(), Some("en0"));
        assert_eq!(parse_route_field(out, "router:"), None);
    }

    #[test]
    fn notify_script_escapes_all_strings() {
        let script = build_notify_script(&json!({
            "title": "Готово \"X\"",
            "message": "a\nb",
            "subtitle": "s\"ub",
            "sound": "Glass",
        }))
        .unwrap();
        assert!(script.starts_with("display notification \"a\\nb\" with title \"Готово \\\"X\\\"\""));
        assert!(script.contains("with subtitle \"s\\\"ub\""));
        assert!(script.contains("with sound name \"Glass\""));
    }

    #[test]
    fn notify_injection_never_breaks_out_of_quotes() {
        let script = build_notify_script(&json!({
            "title": "t\" & do shell script \"rm -rf /",
            "message": "m\" & do shell script \"id",
        }))
        .unwrap();
        assert!(!script.contains("\" & do shell script \""));
    }

    #[test]
    fn notify_requires_non_empty_title_and_message() {
        assert!(build_notify_script(&json!({ "message": "m" })).is_err());
        assert!(build_notify_script(&json!({ "title": "t" })).is_err());
        assert!(build_notify_script(&json!({ "title": "  ", "message": "m" })).is_err());
        assert!(build_notify_script(&json!({ "title": "t", "message": " " })).is_err());
    }

    #[test]
    fn notify_sends_the_escaped_script_and_maps_failures() {
        let dir = temp_test_dir("sys-notify");
        let capture = dir.join("script.txt");
        let fake = fake_bin(&dir, "osascript", &format!("printf '%s' \"$2\" > '{}'", capture.display()));
        let r = NotifyTool::with_binary(fake)
            .execute(&json!({ "title": "T", "message": "M" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["ok"], true);
        assert_eq!(fs::read_to_string(&capture).unwrap(), "display notification \"M\" with title \"T\"");

        let failing = fake_bin(&dir, "osascript-fail", "echo 'nope' >&2\nexit 1");
        let r = NotifyTool::with_binary(failing)
            .execute(&json!({ "title": "T", "message": "M" }), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("nope"), "{}", err.message);

        let r = NotifyTool::with_binary(dir.join("no-osascript"))
            .execute(&json!({ "title": "T", "message": "M" }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_rejects_bad_targets_before_running_anything() {
        let dir = temp_test_dir("sys-open");
        let wd = temp_test_dir("sys-open-wd");
        let tool = OpenTool::with_binary(dir.join("no-open"));
        let ctx = ctx_with_wd(&wd);
        let r = tool.execute(&json!({ "target": "/no/such/file.txt" }), &ctx).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "not_found");
        let r = tool.execute(&json!({ "target": "ftp://example.ru/x" }), &ctx).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        let r = tool.execute(&json!({ "target": "   " }), &ctx).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn open_passes_target_and_app_as_plain_argv() {
        let dir = temp_test_dir("sys-open-argv");
        let wd = temp_test_dir("sys-open-argv-wd");
        let file = wd.join("report.txt");
        fs::write(&file, "x").unwrap();
        let capture = dir.join("argv.txt");
        let fake = fake_bin(&dir, "open", &format!("printf '%s\\n' \"$@\" > '{}'", capture.display()));
        let ctx = ctx_with_wd(&wd);

        let r = OpenTool::with_binary(fake.clone())
            .execute(&json!({ "target": "report.txt" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["target"], file.display().to_string());
        assert_eq!(fs::read_to_string(&capture).unwrap().trim(), file.display().to_string());

        let r = OpenTool::with_binary(fake.clone())
            .execute(&json!({ "target": "https://example.ru/page?q=1", "app": "Safari" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            fs::read_to_string(&capture).unwrap().lines().collect::<Vec<_>>(),
            ["-a", "Safari", "https://example.ru/page?q=1"]
        );

        let r = OpenTool::with_binary(fake)
            .execute(&json!({ "target": format!("file://{}", file.display()) }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(fs::read_to_string(&capture).unwrap().trim(), file.display().to_string());

        let failing = fake_bin(&dir, "open-fail", "echo \"can't open\" >&2\nexit 1");
        let r = OpenTool::with_binary(failing)
            .execute(&json!({ "target": "report.txt" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("can't open"), "{}", err.message);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn event_kind_names_are_stable() {
        assert_eq!(event_kind_name(EventKind::Any), "any");
        assert_eq!(event_kind_name(EventKind::Create(notify::event::CreateKind::File)), "created");
        assert_eq!(event_kind_name(EventKind::Modify(notify::event::ModifyKind::Any)), "modified");
        assert_eq!(event_kind_name(EventKind::Remove(notify::event::RemoveKind::File)), "removed");
        assert_eq!(event_kind_name(EventKind::Access(notify::event::AccessKind::Read)), "accessed");
        assert_eq!(event_kind_name(EventKind::Other), "other");
    }

    #[test]
    fn watch_secs_clamps_to_default_and_max() {
        assert_eq!(watch_secs(None), 10);
        assert_eq!(watch_secs(Some(0)), 1);
        assert_eq!(watch_secs(Some(999)), 60);
        assert_eq!(watch_secs(Some(3)), 3);
    }

    #[test]
    fn fs_watch_rejects_missing_folder() {
        let wd = temp_test_dir("sys-watch-miss");
        let r = FsWatchTool
            .execute(&json!({ "path": "nope" }), &ctx_with_wd(&wd))
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fs_watch_returns_an_empty_list_for_a_quiet_folder() {
        let wd = temp_test_dir("sys-watch-quiet");
        let started = Instant::now();
        let r = FsWatchTool
            .execute(&json!({ "path": ".", "timeout_secs": 1 }), &ctx_with_wd(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["events"].as_array().unwrap().len(), 0);
        assert_eq!(data["count"], 0);
        assert_eq!(data["timed_out"], true);
        assert!(started.elapsed() >= Duration::from_millis(900));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fs_watch_observes_a_created_file_on_the_real_backend() {
        let wd = temp_test_dir("sys-watch-live");
        let wd2 = wd.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..6 {
                std::thread::sleep(Duration::from_millis(400));
                fs::write(wd2.join("new-file.txt"), format!("hello {i}")).unwrap();
            }
        });
        let r = FsWatchTool
            .execute(&json!({ "path": ".", "timeout_secs": 3 }), &ctx_with_wd(&wd))
            .unwrap();
        writer.join().unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let events = data["events"].as_array().unwrap();
        assert!(!events.is_empty(), "no events observed: {data}");
        assert!(events
            .iter()
            .all(|e| e["path"].is_string() && e["kind"].is_string() && e["at"].is_string()));
        let root = wd.display().to_string();
        assert!(events.iter().any(|e| {
            let p = e["path"].as_str().unwrap();
            p.ends_with("new-file.txt") || p == root
        }));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn schemas_reject_unknown_and_missing_properties() {
        assert!(validate_args(&OpenTool::default().parameters(), &json!({ "target": "x", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
        assert!(validate_args(&OpenTool::default().parameters(), &json!({}))
            .unwrap_err()
            .contains("target"));
        assert!(validate_args(&NotifyTool::default().parameters(), &json!({ "title": "t" }))
            .unwrap_err()
            .contains("message"));
        assert!(validate_args(&FsWatchTool.parameters(), &json!({ "path": "p", "timeout_secs": 5 })).is_ok());
        assert!(validate_args(&FsWatchTool.parameters(), &json!({ "path": "p", "timeout_secs": "x" }))
            .unwrap_err()
            .contains("integer"));
        assert!(validate_args(&SysinfoTool.parameters(), &json!({})).is_ok());
        assert!(validate_args(&SysinfoTool.parameters(), &json!({ "anything": 1 }))
            .unwrap_err()
            .contains("anything"));
    }

    #[test]
    fn classes_and_policies_match_the_spec() {
        assert_eq!(SysinfoTool.class(), ToolClass::ReadOnly);
        assert_eq!(SysinfoTool.policy(), PermissionPolicy::Skip);
        assert_eq!(FsWatchTool.class(), ToolClass::ReadOnly);
        assert_eq!(FsWatchTool.policy(), PermissionPolicy::Skip);
        assert_eq!(NotifyTool::default().class(), ToolClass::Mutating);
        assert_eq!(NotifyTool::default().policy(), PermissionPolicy::Skip);
        assert_eq!(OpenTool::default().class(), ToolClass::Mutating);
        assert_eq!(OpenTool::default().policy(), PermissionPolicy::Skip);
    }
}
