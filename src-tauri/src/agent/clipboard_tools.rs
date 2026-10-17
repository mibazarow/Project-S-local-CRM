// macOS clipboard tools via the system pbcopy/pbpaste (no new crates).
// clipboard_get is ReadOnly/Skip, clipboard_set is Mutating/Skip: the
// confirmation queue is not the guard here, the cap and the "never echo
// clipboard contents back" rule are — the pasteboard routinely holds secrets
// copied from other apps, so error messages carry only program names, exit
// codes and (masked) stderr, never the text itself.
//
// Pasteboard isolation: the helpers accept an optional pasteboard name and
// pass `-pboard <name>` to the child, which tests use to address a dedicated
// "ProjectSTest" board. NOTE (verified on macOS 26): the system pbcopy/pbpaste
// silently IGNORE -pboard and always hit the general pasteboard, so the named
// path is only honored by binaries that implement it — tests inject fake
// pbcopy/pbpaste scripts backed by temp files and never touch the user's
// clipboard. Production always passes None (general board, plain pbcopy flags).
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::journal::mask_secrets;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CLIP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_CHARS: usize = 50_000;
const MAX_CHARS_LIMIT: usize = 200_000;
const MAX_SET_BYTES: usize = 1_000_000;
// Envelope cap for a full 200k-char text: serde_json escapes control
// characters as \u00XX (6 bytes per char), so the wrapper must fit the
// worst case and .capped() never false-triggers a valid capped read.
const GET_RESULT_CAP: usize = 1_300_000;
// pbcopy writes nothing to stdout; pbpaste errors are short system noise.
const STDERR_CAP: usize = 8_192;

// Only plain board names reach argv: no option lookalikes, no shell bytes.
fn board_args(pasteboard: Option<&str>) -> std::result::Result<Vec<String>, ToolResult> {
    match pasteboard {
        None => Ok(Vec::new()),
        Some(name) => {
            let safe = !name.is_empty()
                && !name.starts_with('-')
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if safe {
                Ok(vec!["-pboard".to_string(), name.to_string()])
            } else {
                Err(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "pasteboard name must be 1-64 chars of [A-Za-z0-9_-]",
                ))
            }
        }
    }
}

// Reads a pipe up to the cap plus one byte so truncation stays detectable
// while the child keeps producing.
fn read_capped(mut reader: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let _ = (&mut reader).take((cap as u64) + 1).read_to_end(&mut buf);
    let truncated = buf.len() > cap;
    buf.truncate(cap);
    (buf, truncated)
}

// Runs the clipboard helper with piped stdio, an optional stdin payload and a
// hard timeout; returns the capped stdout bytes. Spawn failures map to a
// clear "not found" error naming only the program, never any clipboard data.
fn run_pb(
    program: &Path,
    args: &[String],
    stdin: Option<&[u8]>,
    stdout_cap: usize,
    timeout: Duration,
) -> std::result::Result<(Vec<u8>, bool), ToolResult> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{} not found: the macOS clipboard helper is missing",
                    program.display()
                )
            } else {
                format!("cannot run {}: {e}", program.display())
            };
            return Err(ToolResult::err(ToolErrorCode::ExecutionError, msg));
        }
    };
    let stdin_pipe = child.stdin.take();
    let payload = stdin.map(<[u8]>::to_vec);
    let stdin_thread = std::thread::spawn(move || {
        if let (Some(mut s), Some(bytes)) = (stdin_pipe, payload) {
            let _ = std::io::Write::write_all(&mut s, &bytes);
            let _ = std::io::Write::flush(&mut s);
        }
    });
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, stdout_cap)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, STDERR_CAP)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("cannot wait for {}: {e}", program.display()),
                ));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "{} timed out after {} s and was killed",
                    program.display(),
                    timeout.as_secs_f64()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = stdin_thread.join();
    let (out_bytes, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
    let (err_bytes, _) = err_thread.join().unwrap_or_default().unwrap_or_default();
    if status.code() != Some(0) && !out_cut {
        // stderr is system noise (never the board text); masked anyway, and
        // the clipboard payload itself is never part of any message here.
        // When out_cut is set the child died of SIGPIPE on our closed read
        // pipe: the capped stdout is the intended outcome, not a failure.
        let detail = mask_secrets(&String::from_utf8_lossy(&err_bytes));
        return Err(ToolResult::err(
            ToolErrorCode::ExecutionError,
            format!(
                "{} failed (exit {}): {}",
                program.display(),
                status.code().unwrap_or(-1),
                detail.trim()
            ),
        ));
    }
    Ok((out_bytes, out_cut))
}

// pbpaste: board text as UTF-8 (lossy for stray non-UTF-8 bytes), cut to
// maxChars. An empty or non-text board yields an empty string.
fn pb_paste(
    program: &Path,
    pasteboard: Option<&str>,
    max_chars: usize,
    timeout: Duration,
) -> std::result::Result<(String, bool), ToolResult> {
    let args = board_args(pasteboard)?;
    // UTF-8 needs at most 4 bytes per char, so the byte cap cannot cut a
    // char that still fits inside maxChars.
    let (bytes, byte_cut) = run_pb(program, &args, None, max_chars.saturating_mul(4), timeout)?;
    let full = String::from_utf8_lossy(&bytes);
    let truncated = byte_cut || full.chars().count() > max_chars;
    let text: String = full.chars().take(max_chars).collect();
    Ok((text, truncated))
}

// pbcopy: pipe the text into the helper as stdin.
fn pb_copy(
    program: &Path,
    pasteboard: Option<&str>,
    text: &str,
    timeout: Duration,
) -> std::result::Result<(), ToolResult> {
    let args = board_args(pasteboard)?;
    run_pb(program, &args, Some(text.as_bytes()), STDERR_CAP, timeout)?;
    Ok(())
}

pub struct ClipboardGetTool {
    program: PathBuf,
    timeout: Duration,
}

impl Default for ClipboardGetTool {
    fn default() -> Self {
        ClipboardGetTool {
            program: PathBuf::from("pbpaste"),
            timeout: CLIP_TIMEOUT,
        }
    }
}

impl ClipboardGetTool {
    pub fn with_program(program: PathBuf, timeout: Duration) -> Self {
        ClipboardGetTool { program, timeout }
    }
}

impl Tool for ClipboardGetTool {
    fn name(&self) -> &str {
        "clipboard_get"
    }
    fn description(&self) -> &str {
        "Read the macOS clipboard text as UTF-8 (pbpaste); maxChars 50000 default, 200000 max.\nAn empty or non-text board yields an empty string; clipboard contents never appear in error messages."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "maxChars": { "type": "integer" } },
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
    fn max_result_len(&self) -> usize {
        GET_RESULT_CAP
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let max_chars = args
            .get("maxChars")
            .and_then(Value::as_i64)
            .filter(|v| *v > 0)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_CHARS_LIMIT);
        match pb_paste(&self.program, None, max_chars, self.timeout) {
            Ok((text, truncated)) => Ok(ToolResult::ok(json!({
                "text": text,
                "chars": text.chars().count(),
                "truncated": truncated,
            }))
            .capped(self.max_result_len())),
            Err(r) => Ok(r),
        }
    }
}

pub struct ClipboardSetTool {
    program: PathBuf,
    timeout: Duration,
}

impl Default for ClipboardSetTool {
    fn default() -> Self {
        ClipboardSetTool {
            program: PathBuf::from("pbcopy"),
            timeout: CLIP_TIMEOUT,
        }
    }
}

impl ClipboardSetTool {
    pub fn with_program(program: PathBuf, timeout: Duration) -> Self {
        ClipboardSetTool { program, timeout }
    }
}

impl Tool for ClipboardSetTool {
    fn name(&self) -> &str {
        "clipboard_set"
    }
    fn description(&self) -> &str {
        "Set the macOS clipboard to UTF-8 text (pbcopy); input cap 1 MB.\nPrevious content is lost; nothing is echoed back and clipboard contents never appear in error messages."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
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
        let text = args["text"].as_str().unwrap_or_default();
        if text.len() > MAX_SET_BYTES {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "text is {} bytes, over the {MAX_SET_BYTES}-byte clipboard cap",
                    text.len()
                ),
            ));
        }
        match pb_copy(&self.program, None, text, self.timeout) {
            Ok(()) => Ok(ToolResult::ok(json!({ "bytesWritten": text.len() }))),
            Err(r) => Ok(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // Fake pbcopy/pbpaste honoring -pboard <name> with one store file per
    // board, so tests exercise the real spawn/pipe/cap/timeout paths without
    // ever touching the user's general pasteboard.
    fn fake_clipboard(dir: &Path) -> (PathBuf, PathBuf) {
        let root = dir.display().to_string();
        let copy = dir.join("fake-pbcopy");
        let paste = dir.join("fake-pbpaste");
        fs::write(
            &copy,
            format!(
                "#!/bin/sh\nboard=general\nwhile [ $# -gt 0 ]; do if [ \"$1\" = \"-pboard\" ]; then board=\"$2\"; shift 2; else shift; fi; done\ncat > \"{root}/store-$board\"\n"
            ),
        )
        .unwrap();
        fs::write(
            &paste,
            format!(
                "#!/bin/sh\nboard=general\nwhile [ $# -gt 0 ]; do if [ \"$1\" = \"-pboard\" ]; then board=\"$2\"; shift 2; else shift; fi; done\nf=\"{root}/store-$board\"\nif [ -f \"$f\" ]; then cat \"$f\"; fi\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&paste, fs::Permissions::from_mode(0o755)).unwrap();
        (copy, paste)
    }

    fn board_text(dir: &Path, board: &str) -> PathBuf {
        dir.join(format!("store-{board}"))
    }

    #[test]
    fn class_policy_and_schema_shape() {
        assert_eq!(ClipboardGetTool::default().class(), ToolClass::ReadOnly);
        assert_eq!(ClipboardGetTool::default().policy(), PermissionPolicy::Skip);
        assert_eq!(ClipboardSetTool::default().class(), ToolClass::Mutating);
        assert_eq!(ClipboardSetTool::default().policy(), PermissionPolicy::Skip);

        let get = ClipboardGetTool::default().parameters();
        assert_eq!(get["additionalProperties"], false);
        assert!(validate_args(&get, &json!({})).is_ok());
        assert!(validate_args(&get, &json!({ "maxChars": 1000 })).is_ok());
        assert!(validate_args(&get, &json!({ "maxChars": "x" }))
            .unwrap_err()
            .contains("integer"));
        assert!(validate_args(&get, &json!({ "board": "x" }))
            .unwrap_err()
            .contains("board"));

        let set = ClipboardSetTool::default().parameters();
        assert_eq!(set["additionalProperties"], false);
        assert!(validate_args(&set, &json!({ "text": "hi" })).is_ok());
        assert!(validate_args(&set, &json!({})).unwrap_err().contains("text"));
        assert!(validate_args(&set, &json!({ "text": "hi", "maxChars": 1 }))
            .unwrap_err()
            .contains("maxChars"));
    }

    #[test]
    fn set_get_round_trip_on_dedicated_pasteboard() {
        let dir = temp_test_dir("clip-roundtrip");
        let (copy, paste) = fake_clipboard(&dir);
        let text = "привет, мир!\nsecond line\ttab";
        pb_copy(&copy, Some("ProjectSTest"), text, CLIP_TIMEOUT).unwrap();
        let (got, truncated) = pb_paste(&paste, Some("ProjectSTest"), MAX_CHARS_LIMIT, CLIP_TIMEOUT).unwrap();
        assert_eq!(got, text);
        assert!(!truncated);
        // The general board of the fakes was never written.
        assert!(!board_text(&dir, "general").exists());

        // Spec cleanup: empty input clears the dedicated board.
        pb_copy(&copy, Some("ProjectSTest"), "", CLIP_TIMEOUT).unwrap();
        let (got, truncated) = pb_paste(&paste, Some("ProjectSTest"), MAX_CHARS_LIMIT, CLIP_TIMEOUT).unwrap();
        assert_eq!(got, "");
        assert!(!truncated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn get_caps_default_and_requested_chars() {
        let dir = temp_test_dir("clip-cap");
        let (_copy, paste) = fake_clipboard(&dir);
        // The tool always addresses the general board (production path).
        fs::write(board_text(&dir, "general"), "x".repeat(60_000)).unwrap();

        let get = ClipboardGetTool::with_program(paste.clone(), CLIP_TIMEOUT);
        let r = get
            .execute(&json!({ "maxChars": 100 }), &ToolCtx::default())
            .unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["text"].as_str().unwrap().chars().count(), 100);
        assert_eq!(data["chars"].as_u64().unwrap(), 100);
        assert!(data["truncated"].as_bool().unwrap());

        // Default cap: 50k of the 60k stored.
        let r = get.execute(&json!({}), &ToolCtx::default()).unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["chars"].as_u64().unwrap(), 50_000);
        assert!(data["truncated"].as_bool().unwrap());

        // maxChars above the hard limit clamps to 200k.
        fs::write(board_text(&dir, "general"), "y".repeat(210_000)).unwrap();
        let r = get
            .execute(&json!({ "maxChars": 300_000 }), &ToolCtx::default())
            .unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["chars"].as_u64().unwrap(), 200_000);
        assert!(data["truncated"].as_bool().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_board_is_empty_string() {
        let dir = temp_test_dir("clip-empty");
        let (_, paste) = fake_clipboard(&dir);
        let r = ClipboardGetTool::with_program(paste, CLIP_TIMEOUT)
            .execute(&json!({}), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["text"].as_str().unwrap(), "");
        assert!(!data["truncated"].as_bool().unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn non_utf8_board_content_survives_lossy() {
        // Real pbpaste prints nothing for image/file boards (empty string);
        // raw non-UTF-8 bytes must still not crash the read.
        let dir = temp_test_dir("clip-binary");
        let (_, paste) = fake_clipboard(&dir);
        fs::write(board_text(&dir, "general"), [0xffu8, 0xfe, b'A', 0x00]).unwrap();
        let r = ClipboardGetTool::with_program(paste, CLIP_TIMEOUT)
            .execute(&json!({}), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert!(r.data.unwrap()["text"].as_str().unwrap().contains('A'));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_binary_maps_to_clear_error() {
        let dir = temp_test_dir("clip-missing");
        let missing = dir.join("pbcopy-definitely-missing");
        let set_r = ClipboardSetTool::with_program(missing.clone(), CLIP_TIMEOUT)
            .execute(&json!({ "text": "SECRET-CLIP-9f3a" }), &ToolCtx::default())
            .unwrap();
        let get_r = ClipboardGetTool::with_program(dir.join("pbpaste-definitely-missing"), CLIP_TIMEOUT)
            .execute(&json!({}), &ToolCtx::default())
            .unwrap();
        for r in [set_r, get_r] {
            let err = r.error.unwrap();
            assert_eq!(err.code, "execution_error");
            assert!(err.message.contains("not found"), "{}", err.message);
            assert!(!err.message.contains("SECRET-CLIP"), "{}", err.message);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_rejects_oversized_text_without_running_pbcopy() {
        let dir = temp_test_dir("clip-setcap");
        let (copy, paste) = fake_clipboard(&dir);
        let set = ClipboardSetTool::with_program(copy.clone(), CLIP_TIMEOUT);
        let r = set
            .execute(&json!({ "text": "x".repeat(MAX_SET_BYTES + 1) }), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("1000000"), "{}", err.message);
        // pbcopy never ran: no board file was created.
        assert!(!board_text(&dir, "general").exists());
        // Exactly at the cap is accepted.
        let ok = set
            .execute(&json!({ "text": "x".repeat(MAX_SET_BYTES) }), &ToolCtx::default())
            .unwrap();
        assert!(ok.ok, "{:?}", ok.error);
        // Read back with a byte cap above the stored size so nothing is cut.
        let (got, truncated) = pb_paste(&paste, None, MAX_SET_BYTES + 1, CLIP_TIMEOUT).unwrap();
        assert_eq!(got.chars().count(), MAX_SET_BYTES);
        assert!(!truncated);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn timeout_kills_a_stalled_helper() {
        let dir = temp_test_dir("clip-timeout");
        let stalled = dir.join("stalled-pbpaste");
        fs::write(&stalled, "#!/bin/sh\nsleep 5\n").unwrap();
        fs::set_permissions(&stalled, fs::Permissions::from_mode(0o755)).unwrap();
        let r = ClipboardGetTool::with_program(stalled, Duration::from_millis(300))
            .execute(&json!({}), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("timed out"), "{}", err.message);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsafe_board_names_never_reach_argv() {
        assert!(board_args(Some("ProjectSTest")).is_ok());
        assert!(board_args(Some("bad name")).is_err());
        assert!(board_args(Some("-help")).is_err());
        assert!(board_args(Some("")).is_err());
    }
}
