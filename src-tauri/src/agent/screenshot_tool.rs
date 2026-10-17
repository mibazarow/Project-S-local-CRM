// Screen capture via the macOS `screencapture` CLI (SPEC 5.2 machine-wide
// tool set). Mutating/Check: capturing the screen is sensitive, so every call
// is confirmed. The child always sees TMPDIR/TEMP/TMP pointed at Harness/tmp/,
// default output lands under Harness/tmp/screenshots/, and a 30 s timeout
// kills a stuck capture. An empty or missing output file is reported as a
// missing Screen Recording permission.
use super::harness_tools::resolve_path;
use super::project_tools::require_wd;
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::paths;
use chrono::Local;
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SC_BINARY: &str = "/usr/sbin/screencapture";
const SC_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DELAY_SECS: i64 = 30;
const PERMISSION_HINT: &str = "Grant Screen Recording permission to this app in System Settings → Privacy & Security → Screen Recording, then retry.";

// Builds the screencapture flags (without the output path) from tool args.
// Pure so command construction is testable without running a capture.
fn screencapture_args(args: &Value) -> std::result::Result<Vec<String>, String> {
    let mut out = vec!["-x".to_string()];
    if args.get("includeCursor").and_then(Value::as_bool).unwrap_or(false) {
        out.push("-C".to_string());
    }
    if let Some(delay) = args.get("delaySec") {
        if !delay.is_null() {
            let secs = delay
                .as_i64()
                .ok_or_else(|| "delaySec must be an integer".to_string())?;
            if !(0..=MAX_DELAY_SECS).contains(&secs) {
                return Err(format!("delaySec must be between 0 and {MAX_DELAY_SECS}"));
            }
            if secs > 0 {
                out.push("-T".to_string());
                out.push(secs.to_string());
            }
        }
    }
    let region = args.get("region").filter(|v| !v.is_null());
    let window = args.get("windowId").filter(|v| !v.is_null());
    match (region, window) {
        (Some(_), Some(_)) => Err("region and windowId are mutually exclusive".to_string()),
        (Some(region), None) => {
            let mut parts = Vec::with_capacity(4);
            for key in ["x", "y", "w", "h"] {
                let v = region
                    .get(key)
                    .and_then(Value::as_f64)
                    .ok_or_else(|| format!("region.{key} must be a number"))?;
                parts.push(if v.fract() == 0.0 {
                    (v as i64).to_string()
                } else {
                    v.to_string()
                });
            }
            out.push("-R".to_string());
            out.push(parts.join(","));
            Ok(out)
        }
        (None, Some(window)) => {
            let id = match window {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.trim().parse::<u64>().ok(),
                _ => None,
            };
            match id {
                Some(id) if id > 0 => {
                    out.push("-l".to_string());
                    out.push(id.to_string());
                    Ok(out)
                }
                _ => Err("windowId must be a positive window id (string or number)".to_string()),
            }
        }
        (None, None) => Ok(out),
    }
}

// Default landing path: Harness/tmp/screenshots/shot-YYYYMMDD-HHMMSS.png.
fn default_shot_path(wd: &Path) -> PathBuf {
    paths::harness_tmp_dir(wd)
        .join("screenshots")
        .join(format!("shot-{}.png", Local::now().format("%Y%m%d-%H%M%S")))
}

// Reads a pipe up to the cap so a chatty child can never balloon memory.
fn read_capped(mut reader: impl Read, cap: usize) -> String {
    let mut buf = Vec::new();
    let _ = (&mut reader).take((cap as u64) + 1).read_to_end(&mut buf);
    buf.truncate(cap);
    String::from_utf8_lossy(&buf).into_owned()
}

pub struct ScreenshotTool {
    binary: PathBuf,
    timeout: Duration,
}

impl Default for ScreenshotTool {
    fn default() -> Self {
        ScreenshotTool {
            binary: PathBuf::from(SC_BINARY),
            timeout: SC_TIMEOUT,
        }
    }
}

impl ScreenshotTool {
    // Test seam: a fake binary (missing-binary, timeout, zero-byte cases).
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        ScreenshotTool {
            binary: binary.into(),
            timeout: SC_TIMEOUT,
        }
    }
}

impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "screenshot"
    }
    fn description(&self) -> &str {
        "Capture the macOS screen via the system screencapture CLI (requires Screen Recording permission). Full screen by default; region {x,y,w,h} captures that rectangle, windowId (string or number) captures one window by id — the two are mutually exclusive. includeCursor adds the pointer, delaySec (0..30) waits before capturing. path is the PNG destination (absolute, ~/ or working-dir-relative); parent folders are created; when omitted the shot lands in Harness/tmp/screenshots/shot-YYYYMMDD-HHMMSS.png. Returns {path, bytes}. Always confirmed: screen capture is sensitive."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "region": {
                    "type": "object",
                    "properties": {
                        "x": { "type": "number" },
                        "y": { "type": "number" },
                        "w": { "type": "number" },
                        "h": { "type": "number" }
                    },
                    "required": ["x", "y", "w", "h"],
                    "additionalProperties": false
                },
                "windowId": { "type": ["string", "number"] },
                "delaySec": { "type": "integer", "minimum": 0, "maximum": 30 },
                "includeCursor": { "type": "boolean" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("захват экрана (screencapture)".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let mut cmd_args = match screencapture_args(args) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp)?;
        let out_path = match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => resolve_path(p, &wd),
            _ => default_shot_path(&wd),
        };
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        cmd_args.push(out_path.display().to_string());

        let mut child = match Command::new(&self.binary)
            .args(&cmd_args)
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult::err(
                    ToolErrorCode::NotFound,
                    format!("{} not found; screencapture ships with macOS", self.binary.display()),
                ))
            }
            Err(e) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("cannot run {}: {e}", self.binary.display()),
                ))
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, 4_096)));
        let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, 4_096)));
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!(
                        "screencapture timed out after {} s and was killed",
                        self.timeout.as_secs()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let stdout = out_thread.join().unwrap_or_default().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default().unwrap_or_default();
        if status.code() != Some(0) {
            let detail = if stderr.trim().is_empty() {
                stdout.trim().to_owned()
            } else {
                stderr.trim().to_owned()
            };
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "screencapture failed (exit {}): {detail}. {PERMISSION_HINT}",
                    status.code().unwrap_or(-1)
                ),
            ));
        }
        let bytes = match fs::metadata(&out_path) {
            Ok(m) if m.is_file() && m.len() > 0 => m.len(),
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!(
                        "screencapture exited 0 but wrote no image to {}; the capture produced an empty file. {PERMISSION_HINT}",
                        out_path.display()
                    ),
                ))
            }
        };
        Ok(ToolResult::ok(json!({
            "path": out_path.display().to_string(),
            "bytes": bytes,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use std::os::unix::fs::PermissionsExt;

    fn harness_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        (wd.clone(), ToolCtx {
            working_dir: Some(wd),
            ..Default::default()
        })
    }

    // Fake screencapture: writes $CONTENT into the last argument (the output
    // path) and fails if TMPDIR is not the Harness/tmp we passed.
    fn fake_capture(wd: &Path, content: &str) -> PathBuf {
        let script = wd.join("fake-screencapture.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nlast=\"\"\nfor a in \"$@\"; do last=\"$a\"; done\n[ \"$TMPDIR\" = \"{tmp}\" ] || {{ echo \"TMPDIR is $TMPDIR, expected {tmp}\" >&2; exit 3; }}\nprintf '%s' '{content}' > \"$last\"\n",
                tmp = paths::harness_tmp_dir(wd).display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn class_policy_and_schema_shape() {
        assert_eq!(ScreenshotTool::default().class(), ToolClass::Mutating);
        assert_eq!(ScreenshotTool::default().policy(), PermissionPolicy::Check);
        assert!(ScreenshotTool::default()
            .danger_reason(&json!({}), &ToolCtx::default())
            .is_some());
        let s = ScreenshotTool::default().parameters();
        assert_eq!(s["additionalProperties"], false);
        assert!(validate_args(&s, &json!({})).is_ok());
        assert!(validate_args(&s, &json!({ "region": { "x": 1, "y": 2, "w": 3, "h": 4 } })).is_ok());
        assert!(validate_args(&s, &json!({ "windowId": "12345" })).is_ok());
        assert!(validate_args(&s, &json!({ "windowId": 12345 })).is_ok());
        assert!(validate_args(&s, &json!({ "delaySec": 5, "includeCursor": true })).is_ok());
        assert!(validate_args(&s, &json!({ "bogus": 1 })).unwrap_err().contains("bogus"));
        assert!(validate_args(&s, &json!({ "region": { "x": 1 } }))
            .unwrap_err()
            .contains("y"));
        assert!(validate_args(&s, &json!({ "delaySec": "x" }))
            .unwrap_err()
            .contains("integer"));
    }

    #[test]
    fn full_screen_builds_silent_flags_only() {
        assert_eq!(screencapture_args(&json!({})).unwrap(), ["-x"]);
        assert_eq!(screencapture_args(&json!({ "path": "/tmp/a.png" })).unwrap(), ["-x"]);
    }

    #[test]
    fn region_window_delay_cursor_flags() {
        assert_eq!(
            screencapture_args(&json!({ "region": { "x": 10, "y": 20, "w": 300, "h": 200 } })).unwrap(),
            ["-x", "-R", "10,20,300,200"]
        );
        assert_eq!(
            screencapture_args(&json!({ "region": { "x": 1.5, "y": 2, "w": 3, "h": 4 } })).unwrap(),
            ["-x", "-R", "1.5,2,3,4"]
        );
        assert_eq!(screencapture_args(&json!({ "windowId": 12345 })).unwrap(), ["-x", "-l", "12345"]);
        assert_eq!(screencapture_args(&json!({ "windowId": "42" })).unwrap(), ["-x", "-l", "42"]);
        assert_eq!(
            screencapture_args(&json!({ "delaySec": 3, "includeCursor": true })).unwrap(),
            ["-x", "-C", "-T", "3"]
        );
        // delaySec 0 means "no wait": -T is omitted
        assert_eq!(screencapture_args(&json!({ "delaySec": 0 })).unwrap(), ["-x"]);
        assert_eq!(screencapture_args(&json!({ "delaySec": 30 })).unwrap(), ["-x", "-T", "30"]);
    }

    #[test]
    fn invalid_combinations_are_rejected() {
        let err = screencapture_args(&json!({ "region": { "x": 0, "y": 0, "w": 1, "h": 1 }, "windowId": 7 }))
            .unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
        assert!(screencapture_args(&json!({ "region": { "x": 0, "y": 0, "w": 1 } }))
            .unwrap_err()
            .contains("region.h"));
        assert!(screencapture_args(&json!({ "windowId": "abc" })).unwrap_err().contains("windowId"));
        assert!(screencapture_args(&json!({ "windowId": 0 })).unwrap_err().contains("windowId"));
        assert!(screencapture_args(&json!({ "delaySec": 31 })).unwrap_err().contains("delaySec"));
        assert!(screencapture_args(&json!({ "delaySec": -1 })).unwrap_err().contains("delaySec"));
    }

    #[test]
    fn default_shot_path_pattern_under_harness_tmp() {
        let p = default_shot_path(Path::new("/wd"));
        assert_eq!(
            p.parent().unwrap(),
            PathBuf::from("/wd/Harness/tmp/screenshots")
        );
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("shot-") && name.ends_with(".png"), "{name}");
        let stem = &name["shot-".len()..name.len() - ".png".len()];
        assert_eq!(stem.len(), 15, "{stem}");
        assert_eq!(stem.matches(|c: char| c.is_ascii_digit()).count(), 14, "{stem}");
        assert_eq!(&stem[8..9], "-", "{stem}");
    }

    #[test]
    fn fake_capture_creates_parent_dirs_returns_bytes_and_passes_tmpdir() {
        let (wd, ctx) = harness_env("shot-fake");
        let tool = ScreenshotTool::with_binary(fake_capture(&wd, "fakepng"));
        let r = tool
            .execute(&json!({ "path": "Harness/custom/nested/shot.png" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["bytes"].as_u64().unwrap(), 7);
        let written = wd.join("Harness/custom/nested/shot.png");
        assert_eq!(fs::read_to_string(&written).unwrap(), "fakepng");

        // No path: default pattern under Harness/tmp/screenshots/.
        let r = tool.execute(&json!({}), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let path = PathBuf::from(data["path"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), paths::harness_tmp_dir(&wd).join("screenshots"));
        assert!(path.file_name().unwrap().to_string_lossy().starts_with("shot-"));
        assert!(data["bytes"].as_u64().unwrap() > 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_binary_maps_to_not_found() {
        let (wd, ctx) = harness_env("shot-nobinary");
        let r = ScreenshotTool::with_binary(wd.join("no-such-screencapture"))
            .execute(&json!({}), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("screencapture"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timeout_kills_a_stuck_capture() {
        let (wd, ctx) = harness_env("shot-timeout");
        let script = wd.join("hang.sh");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let r = ScreenshotTool {
            binary: script,
            timeout: Duration::from_millis(300),
        }
        .execute(&json!({}), &ctx)
        .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("timed out"), "{}", err.message);
        assert!(started.elapsed() < Duration::from_secs(10));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn zero_byte_output_mentions_screen_recording_permission() {
        let (wd, ctx) = harness_env("shot-zero");
        let r = ScreenshotTool::with_binary(fake_capture(&wd, ""))
            .execute(&json!({ "path": "Harness/tmp/screenshots/empty.png" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("Screen Recording"), "{}", err.message);
        assert!(err.message.contains("Privacy & Security"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn nonzero_exit_reports_stderr_and_permission_hint() {
        let (wd, ctx) = harness_env("shot-exit");
        let script = wd.join("fail.sh");
        fs::write(&script, "#!/bin/sh\necho 'cannot capture' >&2\nexit 2\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let r = ScreenshotTool::with_binary(&script).execute(&json!({}), &ctx).unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("cannot capture"), "{}", err.message);
        assert!(err.message.contains("Screen Recording"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    #[ignore = "captures the real screen; needs Screen Recording permission"]
    fn live_capture_of_a_small_region_writes_a_png() {
        if !Path::new(SC_BINARY).is_file() {
            eprintln!("no screencapture binary on this machine, skipping live test");
            return;
        }
        let (wd, ctx) = harness_env("shot-live");
        let r = ScreenshotTool::default()
            .execute(&json!({ "region": { "x": 0, "y": 0, "w": 64, "h": 64 } }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert!(data["bytes"].as_u64().unwrap() > 0);
        assert!(PathBuf::from(data["path"].as_str().unwrap()).is_file());
        fs::remove_dir_all(&wd).unwrap();
    }
}
