// OCR tool: extracts text from images and PDFs with the macOS Vision
// framework through a small embedded Swift helper (Vision + AppKit + PDFKit),
// so no new Rust crates are needed. ReadOnly/Skip in both catalogs: the tool
// only reads the target file and writes its helper script under Harness/tmp/.
// The swift child process gets TMPDIR/TEMP/TMP pointed at Harness/tmp, a
// 180 s timeout and capped stdout/stderr; it never touches the network.
use super::harness_tools::resolve_path;
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{read_capped, ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::{fs_ops, paths};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OCR_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_MAX_CHARS: usize = 50_000;
const MAX_CHARS_LIMIT: usize = 50_000;
// Envelope cap leaves room for the JSON wrapper around a full 50k text.
const RESULT_CAP: usize = 52_000;
const OCR_EXTS: [&str; 9] = ["png", "jpg", "jpeg", "heic", "tiff", "bmp", "gif", "webp", "pdf"];

// Swift helper source, written to Harness/tmp/ocr.swift on demand. Images go
// straight through NSImage; PDF pages are rendered at ~2x their media box
// because Vision reads rasterized pages far better than vector text.
const OCR_SWIFT: &str = r#"import AppKit
import Foundation
import PDFKit
import Vision

func die(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

guard CommandLine.arguments.count >= 2 else {
    die("usage: ocr.swift <path>")
}
let path = CommandLine.arguments[1]
let url = URL(fileURLWithPath: path)

func ocrLines(_ cgImage: CGImage) -> [String] {
    var lines: [String] = []
    var requestError: Error?
    let request = VNRecognizeTextRequest { req, err in
        if let err = err {
            requestError = err
            return
        }
        for case let obs as VNRecognizedTextObservation in req.results ?? [] {
            if let top = obs.topCandidates(1).first {
                lines.append(top.string)
            }
        }
    }
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true
    request.recognitionLanguages = ["ru-RU", "en-US"]
    let handler = VNImageRequestHandler(cgImage: cgImage, options: [:])
    do {
        try handler.perform([request])
    } catch {
        die("OCR failed for \(path): \(error.localizedDescription)")
    }
    if let requestError = requestError {
        die("OCR failed for \(path): \(requestError.localizedDescription)")
    }
    return lines
}

func lines(from image: NSImage) -> [String] {
    guard let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
        die("cannot decode image data at \(path)")
    }
    return ocrLines(cg)
}

var out: [String] = []
if url.pathExtension.lowercased() == "pdf" {
    guard let doc = PDFDocument(url: url) else {
        die("cannot open PDF at \(path)")
    }
    for index in 0..<doc.pageCount {
        guard let page = doc.page(at: index) else { continue }
        let media = page.bounds(for: .mediaBox)
        let size = NSSize(width: max(1, media.width * 2), height: max(1, media.height * 2))
        out.append("--- page \(index + 1) ---")
        out.append(contentsOf: lines(from: page.thumbnail(of: size, for: .mediaBox)))
    }
} else {
    guard let image = NSImage(contentsOfFile: path) else {
        die("cannot load image at \(path)")
    }
    out.append(contentsOf: lines(from: image))
}
print(out.joined(separator: "\n"))
"#;

// Toolchain discovery: an explicit $PROJECT_S_SWIFT override (also how tests
// simulate a missing toolchain), then `which swift`, then `xcrun --find swift`
// (the latter covers CLT-only installs where swift is not on PATH).
pub(crate) fn find_swift() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("PROJECT_S_SWIFT") {
        let path = PathBuf::from(raw);
        return path.is_file().then_some(path);
    }
    run_locator("which", &["swift"]).or_else(|| run_locator("xcrun", &["--find", "swift"]))
}

fn run_locator(program: &str, args: &[&str]) -> Option<PathBuf> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    path.is_file().then_some(path)
}

pub struct OcrTool;

impl Tool for OcrTool {
    fn name(&self) -> &str {
        "ocr"
    }
    fn description(&self) -> &str {
        "Extract text from an image or PDF with the macOS Vision framework (accurate recognition, ru-RU + en-US) run through a Swift helper; requires the Xcode Command Line Tools. path must point at an existing file and is project-relative (project agent) or absolute/~/working-dir-relative (harness agent). Supported extensions: png, jpg, jpeg, heic, tiff, bmp, gif, webp, pdf. PDF pages are rendered at ~2x and each page's text is prefixed with \"--- page N ---\". maxChars caps the returned text (default and maximum 50000)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "maxChars": { "type": "integer" }
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
    fn max_result_len(&self) -> usize {
        RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let raw = args["path"].as_str().unwrap_or_default();
        let (label, path) = match ctx.project_id {
            Some(pid) => match with_conn(ctx, |conn| fs_ops::resolve_existing(conn, &wd, pid, raw)) {
                Ok(path) => (raw.to_owned(), path),
                Err(e) => return Ok(envelope::<()>(Err(e))),
            },
            None => {
                let path = resolve_path(raw, &wd);
                let path = match super::harness_tools::lenient_existing(&path) {
                    Ok(p) => p,
                    Err(e) => return Ok(super::harness_tools::lenient_fail(&e)),
                };
                match fs::metadata(&path) {
                    Ok(m) if m.is_file() => (path.display().to_string(), path),
                    Ok(_) => {
                        return Ok(ToolResult::err(
                            ToolErrorCode::InvalidArgs,
                            format!("{} is a directory", path.display()),
                        ))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(ToolResult::err(
                            ToolErrorCode::NotFound,
                            format!("{} not found", path.display()),
                        ))
                    }
                    Err(e) => {
                        return Ok(ToolResult::err(
                            ToolErrorCode::ExecutionError,
                            format!("{}: {e}", path.display()),
                        ))
                    }
                }
            }
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !OCR_EXTS.contains(&ext.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "\"{}\" has unsupported extension \"{ext}\"; supported: {}",
                    path.display(),
                    OCR_EXTS.join(", ")
                ),
            ));
        }
        let max_chars = args
            .get("maxChars")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_CHARS_LIMIT);

        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp)?;
        let script = tmp.join("ocr.swift");
        if fs::read_to_string(&script).unwrap_or_default() != OCR_SWIFT {
            fs::write(&script, OCR_SWIFT)?;
        }
        let swift = match find_swift() {
            Some(p) => p,
            None => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    "swift toolchain not found (checked $PROJECT_S_SWIFT, `which swift`, `xcrun --find swift`); OCR needs the Xcode Command Line Tools — install them with `xcode-select --install`",
                ))
            }
        };

        let mut child = match Command::new(&swift)
            .arg(&script)
            .arg(&path)
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("cannot run {}: {e}", swift.display()),
                ))
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // UTF-8 needs at most 4 bytes per char, so the byte cap cannot cut a
        // char that still fits inside maxChars.
        let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, max_chars * 4).0));
        let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, 8_192).0));
        let deadline = Instant::now() + OCR_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("OCR timed out after {} s and was killed", OCR_TIMEOUT.as_secs()),
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
                format!("OCR failed (exit {}): {detail}", status.code().unwrap_or(-1)),
            ));
        }

        let text = stdout.trim_end_matches(['\n', '\r']);
        // Page markers are counted before the cap so `pages` describes the
        // document, not the truncated tail.
        let pages = (ext == "pdf")
            .then(|| text.lines().filter(|l| l.starts_with("--- page ")).count());
        let mut text: String = text.chars().take(max_chars).collect();
        if text.ends_with('\n') {
            text.truncate(text.trim_end_matches(['\n', '\r']).len());
        }
        let chars = text.chars().count();
        let mut data = json!({ "path": label, "text": text, "chars": chars });
        if let Some(pages) = pages {
            data["pages"] = json!(pages);
        }
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::doc_tools::CreateDocumentTool;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::state::AppState;
    use std::sync::Arc;

    fn project_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            project_id: Some(dto.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        (paths::projects_dir(&wd).join("proj"), ctx)
    }

    fn harness_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::harness_dir(&wd)).unwrap();
        (wd.clone(), ToolCtx {
            working_dir: Some(wd),
            ..Default::default()
        })
    }

    #[test]
    fn class_policy_and_schema_shape() {
        assert_eq!(OcrTool.class(), ToolClass::ReadOnly);
        assert_eq!(OcrTool.policy(), PermissionPolicy::Skip);
        let s = OcrTool.parameters();
        assert_eq!(s["type"], "object");
        assert_eq!(s["additionalProperties"], false);
        assert!(validate_args(&s, &json!({ "path": "a.png" })).is_ok());
        assert!(validate_args(&s, &json!({ "path": "a.pdf", "maxChars": 1000 })).is_ok());
        assert!(validate_args(&s, &json!({})).unwrap_err().contains("path"));
        assert!(validate_args(&s, &json!({ "path": "a.png", "maxChars": "x" }))
            .unwrap_err()
            .contains("integer"));
    }

    #[test]
    fn project_paths_stay_inside_and_missing_files_are_not_found() {
        let (root, ctx) = project_env("ocr-escape");
        for bad in ["../x.png", "/etc/passwd", "a/../../x.png", ".."] {
            let r = OcrTool.execute(&json!({ "path": bad }), &ctx).unwrap();
            assert!(!r.ok, "accepted {bad:?}");
            assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
        }
        let r = OcrTool.execute(&json!({ "path": "missing.png" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");

        fs::create_dir_all(root.join("folder.png")).unwrap();
        let r = OcrTool.execute(&json!({ "path": "folder.png" }), &ctx).unwrap();
        // project scope maps "is a folder" through the shared envelope, the
        // same as read_file/list_tree do
        assert_eq!(r.error.unwrap().code, "execution_error");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn harness_paths_resolve_absolute_and_relative_missing_is_not_found() {
        let (wd, ctx) = harness_env("ocr-harness");
        let dir = wd.join("Harness").join("shots");
        fs::create_dir_all(&dir).unwrap();

        let r = OcrTool.execute(&json!({ "path": "Harness/shots/nope.png" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        let abs = dir.join("nope.png").display().to_string();
        let r = OcrTool.execute(&json!({ "path": abs }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");

        let r = OcrTool.execute(&json!({ "path": "Harness/shots" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn unsupported_extension_is_rejected_before_running_swift() {
        let (root, ctx) = project_env("ocr-ext");
        fs::write(root.join("notes.txt"), "text").unwrap();
        let r = OcrTool.execute(&json!({ "path": "notes.txt" }), &ctx).unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("unsupported extension"), "{}", err.message);
        assert!(err.message.contains("png") && err.message.contains("pdf"), "{}", err.message);
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn helper_script_is_written_under_harness_tmp_and_rewritten_when_stale() {
        let (wd, ctx) = harness_env("ocr-script");
        let dir = wd.join("Harness").join("shots");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pic.png"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
        // The run itself may fail on a machine without the swift toolchain;
        // the helper script must exist under Harness/tmp either way.
        let _ = OcrTool.execute(&json!({ "path": "Harness/shots/pic.png" }), &ctx);
        let script = paths::harness_tmp_dir(&wd).join("ocr.swift");
        assert_eq!(fs::read_to_string(&script).unwrap(), OCR_SWIFT);

        fs::write(&script, "stale").unwrap();
        let _ = OcrTool.execute(&json!({ "path": "Harness/shots/pic.png" }), &ctx);
        assert_eq!(fs::read_to_string(&script).unwrap(), OCR_SWIFT);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_swift_toolchain_reports_clear_error() {
        let (wd, ctx) = harness_env("ocr-noswift");
        let dir = wd.join("Harness").join("shots");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pic.png"), [0x89u8, b'P', b'N', b'G', 0, 1]).unwrap();
        std::env::set_var("PROJECT_S_SWIFT", wd.join("no-such-swift"));
        let r = OcrTool.execute(&json!({ "path": "Harness/shots/pic.png" }), &ctx).unwrap();
        std::env::remove_var("PROJECT_S_SWIFT");
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("Xcode Command Line Tools"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    #[ignore = "runs the real swift+Vision toolchain end to end"]
    fn live_ocr_reads_text_from_a_generated_pdf() {
        let Some(swift) = find_swift() else {
            eprintln!("no swift toolchain on this machine, skipping live OCR test");
            return;
        };
        let (root, ctx) = project_env("ocr-live");
        let r = CreateDocumentTool::project()
            .execute(
                &json!({
                    "path": "ocr-sample.pdf",
                    "format": "pdf",
                    "title": "OCR sample",
                    "markdown": "The quick brown fox jumps over the lazy dog.\n\nБыстрая коричневая лиса прыгает через ленивую собаку.\n",
                }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "create_document failed: {:?}", r.error);
        assert!(root.join("ocr-sample.pdf").is_file());

        let r = OcrTool.execute(&json!({ "path": "ocr-sample.pdf" }), &ctx).unwrap();
        assert!(r.ok, "swift {}: {:?}", swift.display(), r.error);
        let data = r.data.unwrap();
        let text = data["text"].as_str().unwrap().to_owned();
        eprintln!("live OCR text: {text}");
        assert_eq!(data["pages"].as_u64().unwrap(), 1);
        assert!(data["chars"].as_u64().unwrap() > 10);
        assert!(text.contains("quick brown fox"), "OCR text: {text}");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }
}
