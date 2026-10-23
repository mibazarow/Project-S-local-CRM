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
use super::result::{read_capped_bytes, ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use base64::Engine as _;
use chrono::Local;
use crate::error::{AppError, Result};
use crate::journal::mask_secrets;
use crate::paths;
use serde_json::{json, Value};
use std::fs;
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
    let out_thread =
        std::thread::spawn(move || stdout.map(|r| read_capped_bytes(r, stdout_cap)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped_bytes(r, STDERR_CAP)));
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
    #[cfg(test)]
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
    #[cfg(test)]
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

const OSASCRIPT_BINARY: &str = "/usr/bin/osascript";
const SIPS_BINARY: &str = "/usr/bin/sips";
const PNGPASTE_CANDIDATES: [&str; 2] = ["/opt/homebrew/bin/pngpaste", "/usr/local/bin/pngpaste"];
const IMAGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const DATA_URL_MAX_BYTES: u64 = 4 * 1024 * 1024;
const NO_IMAGE_MESSAGE: &str = "В буфере обмена нет изображения: скопируйте картинку (например, Cmd+Ctrl+Shift+4 делает скриншот в буфер) и повторите.";

fn clip_image_script(raw_path: &Path) -> String {
    format!(
        "set out to {}\ntry\n\tset theData to (the clipboard as \u{ab}class PNGf\u{bb})\n\tset fmt to \"png\"\non error\n\ttry\n\t\tset theData to (the clipboard as \u{ab}class TIFF\u{bb})\n\t\tset fmt to \"tiff\"\n\ton error\n\t\treturn \"NO_IMAGE\"\n\tend try\nend try\nset fRef to open for access POSIX file out with write permission\nwrite theData to fRef\nclose access POSIX file out\nreturn fmt",
        super::macos_ui_tool::as_quote(&raw_path.display().to_string())
    )
}

fn run_pngpaste(binary: &Path, out: &Path, timeout: Duration) -> std::result::Result<(), String> {
    let mut child = match Command::new(binary)
        .arg(out.display().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{} not found", binary.display()))
        }
        Err(e) => return Err(format!("cannot run {}: {e}", binary.display())),
    };
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped_bytes(r, 4_096).0));
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed to wait for {}: {e}", binary.display()));
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
    }
    let _ = err_thread.join();
    Ok(())
}

pub struct ClipboardGetImageTool {
    osascript: PathBuf,
    pngpaste: Option<PathBuf>,
    sips: PathBuf,
    timeout: Duration,
}

impl Default for ClipboardGetImageTool {
    fn default() -> Self {
        ClipboardGetImageTool {
            osascript: PathBuf::from(OSASCRIPT_BINARY),
            pngpaste: PNGPASTE_CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file()),
            sips: PathBuf::from(SIPS_BINARY),
            timeout: IMAGE_TIMEOUT,
        }
    }
}

impl ClipboardGetImageTool {
    #[cfg(test)]
    pub fn with_programs(
        osascript: impl Into<PathBuf>,
        pngpaste: Option<impl Into<PathBuf>>,
        sips: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Self {
        ClipboardGetImageTool {
            osascript: osascript.into(),
            pngpaste: pngpaste.map(Into::into),
            sips: sips.into(),
            timeout,
        }
    }
}

impl Tool for ClipboardGetImageTool {
    fn name(&self) -> &str {
        "clipboard_get_image"
    }
    fn description(&self) -> &str {
        "Read an image (PNG or TIFF) from the macOS clipboard and save it as PNG under Settings/tmp/clipboard-<timestamp>.png; read-only, never modifies the clipboard.\nReturns {path, mime, bytes, width, height}; asDataUrl adds a base64 data URL for images up to 4 MB. Uses osascript/System Events with an optional pngpaste fallback; no image on the pasteboard is a clear error, permission failures name the System Settings pane to fix."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "asDataUrl": { "type": "boolean" } },
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
        6_000_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let as_data_url = args
            .get("asDataUrl")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let dir = paths::settings_tmp_dir(&wd);
        fs::create_dir_all(&dir)?;
        let stamp = Local::now().format("%Y%m%d-%H%M%S-%3f");
        let out = dir.join(format!("clipboard-{stamp}.png"));
        let raw = dir.join(format!("clipboard-{stamp}.raw"));

        let mut permission_failure: Option<ToolResult> = None;
        let mut converted_from_tiff = false;
        if self.osascript.is_file() {
            let script = clip_image_script(&raw);
            match super::macos_ui_tool::run_osascript(&self.osascript, &script, self.timeout) {
                Ok((Some(0), stdout, _)) => match stdout.trim() {
                    "NO_IMAGE" => {
                        return Ok(ToolResult::err(ToolErrorCode::NotFound, NO_IMAGE_MESSAGE))
                    }
                    "png" => fs::rename(&raw, &out)?,
                    "tiff" => {
                        let argv = vec![
                            "-s".to_string(),
                            "format".to_string(),
                            "png".to_string(),
                            raw.display().to_string(),
                            "--out".to_string(),
                            out.display().to_string(),
                        ];
                        let (code, _, stderr) = match super::image_tools::run_sips(
                            &self.sips,
                            &argv,
                            &dir,
                            self.timeout,
                        ) {
                            Ok(runs) => runs,
                            Err(e) => {
                                let _ = fs::remove_file(&raw);
                                return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e));
                            }
                        };
                        let _ = fs::remove_file(&raw);
                        if code != Some(0) {
                            return Ok(ToolResult::err(
                                ToolErrorCode::ExecutionError,
                                format!(
                                    "sips could not convert the clipboard TIFF to PNG (exit {}): {}",
                                    code.unwrap_or(-1),
                                    stderr.trim()
                                ),
                            ));
                        }
                        converted_from_tiff = true;
                    }
                    other => {
                        return Ok(ToolResult::err(
                            ToolErrorCode::ExecutionError,
                            format!("unexpected osascript answer \"{other}\" from the clipboard image script"),
                        ))
                    }
                },
                Ok((code, _, stderr)) => {
                    let mapped = super::macos_ui_tool::map_osascript_failure(code, &stderr);
                    if mapped.error.as_ref().is_some_and(|e| e.code == "permission_denied") {
                        permission_failure = Some(mapped);
                    } else {
                        return Ok(mapped);
                    }
                }
                Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
            }
        }
        if !out.is_file() {
            match &self.pngpaste {
                Some(bin) => {
                    if let Err(e) = run_pngpaste(bin, &out, self.timeout) {
                        return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e));
                    }
                }
                None => {
                    if let Some(mapped) = permission_failure {
                        return Ok(mapped);
                    }
                    return Ok(ToolResult::err(
                        ToolErrorCode::NotFound,
                        format!(
                            "{} not found; osascript ships with every macOS install (pngpaste is an optional fallback)",
                            self.osascript.display()
                        ),
                    ));
                }
            }
        }
        let bytes = match fs::metadata(&out) {
            Ok(m) if m.is_file() && m.len() > 0 => m.len(),
            _ => {
                let _ = fs::remove_file(&out);
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("the clipboard image read produced no PNG at {}", out.display()),
                ));
            }
        };
        if bytes > MAX_IMAGE_BYTES {
            let _ = fs::remove_file(&out);
            return Ok(ToolResult::err(
                ToolErrorCode::LimitExceeded,
                format!(
                    "clipboard image is {bytes} bytes, over the {MAX_IMAGE_BYTES}-byte cap"
                ),
            ));
        }
        let mut data = json!({
            "path": out.display().to_string(),
            "mime": "image/png",
            "bytes": bytes,
        });
        if converted_from_tiff {
            data["convertedFrom"] = json!("tiff");
        }
        if let Some((w, h)) = super::image_tools::query_size(&self.sips, &dir, &out) {
            data["width"] = json!(w);
            data["height"] = json!(h);
        }
        if as_data_url {
            if bytes <= DATA_URL_MAX_BYTES {
                let png = fs::read(&out)?;
                data["dataUrl"] = json!(format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(png)
                ));
            } else {
                data["dataUrlSkipped"] = json!(format!(
                    "image is {bytes} bytes, over the {DATA_URL_MAX_BYTES}-byte data-url cap"
                ));
            }
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

#[cfg(all(test, unix))]
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

    fn image_ctx(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        (wd.clone(), ToolCtx {
            working_dir: Some(wd),
            ..Default::default()
        })
    }

    fn fake_osascript(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("fake-osascript");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn fake_binary(dir: &Path, name: &str, body: &str) -> PathBuf {
        let script = dir.join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn extract_raw_path_body(write_cmd: &str) -> String {
        format!(
            "path=$(printf '%s' \"$2\" | sed -n 's/^set out to \"\\(.*\\)\".*/\\1/p')\n{write_cmd}"
        )
    }

    #[test]
    fn image_tool_class_policy_and_schema_shape() {
        let tool = ClipboardGetImageTool::default();
        assert_eq!(tool.class(), ToolClass::ReadOnly);
        assert_eq!(tool.policy(), PermissionPolicy::Skip);
        let schema = tool.parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert!(validate_args(&schema, &json!({})).is_ok());
        assert!(validate_args(&schema, &json!({ "asDataUrl": true })).is_ok());
        assert!(validate_args(&schema, &json!({ "asDataUrl": "yes" }))
            .unwrap_err()
            .contains("boolean"));
        assert!(validate_args(&schema, &json!({ "path": "x" }))
            .unwrap_err()
            .contains("path"));
    }

    #[test]
    fn png_board_saved_under_settings_tmp_with_size_and_data_url() {
        let (wd, ctx) = image_ctx("clipimg-png");
        let fake = fake_osascript(
            &wd,
            &format!(
                "{}\necho png",
                extract_raw_path_body(&format!("printf '{TINY_PNG_B64}' | base64 -D > \"$path\""))
            ),
        );
        let tool = ClipboardGetImageTool::with_programs(&fake, None::<PathBuf>, SIPS_BINARY, CLIP_TIMEOUT);
        let r = tool
            .execute(&json!({ "asDataUrl": true }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["mime"], "image/png");
        let path = PathBuf::from(data["path"].as_str().unwrap());
        assert_eq!(path.parent().unwrap(), paths::settings_tmp_dir(&wd));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("clipboard-") && name.ends_with(".png"), "{name}");
        assert_eq!(data["bytes"].as_u64().unwrap(), 70);
        assert_eq!(data["width"].as_u64(), Some(1));
        assert_eq!(data["height"].as_u64(), Some(1));
        assert!(data["dataUrl"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        assert!(path.is_file());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn empty_board_yields_russian_no_image_error() {
        let (wd, ctx) = image_ctx("clipimg-noimage");
        let fake = fake_osascript(&wd, "echo NO_IMAGE");
        let r = ClipboardGetImageTool::with_programs(&fake, None::<PathBuf>, SIPS_BINARY, CLIP_TIMEOUT)
            .execute(&json!({}), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("В буфере обмена нет изображения"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn automation_denial_maps_to_permission_error_without_fallback() {
        let (wd, ctx) = image_ctx("clipimg-permission");
        let fake = fake_osascript(
            &wd,
            "echo 'Не разрешена отправка событий Apple от «osascript» к «System Events». (-1743)' >&2\nexit 1",
        );
        let r = ClipboardGetImageTool::with_programs(&fake, None::<PathBuf>, SIPS_BINARY, CLIP_TIMEOUT)
            .execute(&json!({}), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "permission_denied");
        assert!(err.message.contains("Автоматизация"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn pngpaste_fallback_recovers_after_permission_denial() {
        let (wd, ctx) = image_ctx("clipimg-pngpaste");
        let fake = fake_osascript(
            &wd,
            "echo 'osascript is not allowed assistive access. (-1719)' >&2\nexit 1",
        );
        let paste = fake_binary(
            &wd,
            "fake-pngpaste",
            &format!("printf '{TINY_PNG_B64}' | base64 -D > \"$1\""),
        );
        let r = ClipboardGetImageTool::with_programs(&fake, Some(paste), SIPS_BINARY, CLIP_TIMEOUT)
            .execute(&json!({}), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["bytes"].as_u64().unwrap(), 70);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_osascript_and_pngpaste_maps_to_not_found() {
        let (wd, ctx) = image_ctx("clipimg-nobinary");
        let r = ClipboardGetImageTool::with_programs(
            wd.join("no-osascript"),
            None::<PathBuf>,
            SIPS_BINARY,
            CLIP_TIMEOUT,
        )
        .execute(&json!({}), &ctx)
        .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("osascript"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn tiff_board_converts_via_sips() {
        let (wd, ctx) = image_ctx("clipimg-tiff");
        let fake = fake_osascript(
            &wd,
            &format!(
                "{}\necho tiff",
                extract_raw_path_body("printf 'II-star-garbage' > \"$path\"")
            ),
        );
        let sips = fake_binary(
            &wd,
            "fake-sips-ok",
            &format!(
                "out=\"\"; prev=\"\"; for a in \"$@\"; do if [ \"$prev\" = \"--out\" ]; then out=\"$a\"; fi; prev=\"$a\"; done\nprintf '{TINY_PNG_B64}' | base64 -D > \"$out\""
            ),
        );
        let r = ClipboardGetImageTool::with_programs(&fake, None::<PathBuf>, sips, CLIP_TIMEOUT)
            .execute(&json!({}), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["convertedFrom"], "tiff");
        assert_eq!(data["bytes"].as_u64().unwrap(), 70);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn tiff_conversion_failure_names_sips() {
        let (wd, ctx) = image_ctx("clipimg-tiff-fail");
        let fake = fake_osascript(
            &wd,
            &format!(
                "{}\necho tiff",
                extract_raw_path_body("printf 'garbage' > \"$path\"")
            ),
        );
        let sips = fake_binary(&wd, "fake-sips-fail", "echo 'Error: Cannot parse file' >&2\nexit 1");
        let r = ClipboardGetImageTool::with_programs(&fake, None::<PathBuf>, sips, CLIP_TIMEOUT)
            .execute(&json!({}), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("sips"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }
}
