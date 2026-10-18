// PDF operations (merge / split / fill form) through an embedded Swift helper
// built on Apple PDFKit, following the ocr_tools.rs pattern: the helper lives
// under Harness/tmp/, the swift child sees TMPDIR/TEMP/TMP pointed there, runs
// under a timeout with capped stdout/stderr pipes and takes its job as a JSON
// config on stdin (errors go to stderr with a non-zero exit, the result JSON
// goes to stdout). No new Rust dependencies.
// Mutating/CheckConditional: a new output inside the project folder (project
// agent) or inside Harness/ and Projects/ (harness agent) skips confirmation;
// anything else — and every call that replaces an existing file — confirms.
// Existing outputs are only replaced with an explicit overwrite=true. Inputs
// and outputs are resolved by scope: project-relative through the fs_ops
// resolvers, otherwise absolute/~/working-dir-relative.
use super::harness_tools::{canonicalish, in_default_territory, resolve_path};
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::{fs_ops, paths, projects};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PDF_TIMEOUT: Duration = Duration::from_secs(120);
const STDOUT_CAP: usize = 64_000;
const STDERR_CAP: usize = 8_192;

// Swift helper source, written to Harness/tmp/pdf_tools.swift on demand. The
// job arrives as JSON on stdin: {"mode":"merge","inputs":[...],"output":...},
// {"mode":"split","input":...,"outputDir":...,"ranges":[{"from":1,"to":2}]}
// or {"mode":"fill","input":...,"output":...,"fields":{name:value}}.
const PDF_SWIFT: &str = r#"import Foundation
import PDFKit

func die(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

func emit(_ payload: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: payload) else {
        die("cannot encode the result as JSON")
    }
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([0x0A]))
}

let stdinData = FileHandle.standardInput.readDataToEndOfFile()
guard let config = (try? JSONSerialization.jsonObject(with: stdinData)) as? [String: Any] else {
    die("expected a JSON config object on stdin")
}
let mode = config["mode"] as? String ?? ""

func requireText(_ key: String) -> String {
    guard let value = config[key] as? String, !value.isEmpty else {
        die("\(mode): missing string \"\(key)\"")
    }
    return value
}

// Documents are loaded fully into memory (PDFDocument(data:)) so writing the
// output over a still-open input path cannot corrupt a lazy page read.
func loadDoc(_ path: String) -> PDFDocument {
    guard let data = try? Data(contentsOf: URL(fileURLWithPath: path)),
          let doc = PDFDocument(data: data) else {
        die("cannot open PDF at \(path)")
    }
    return doc
}

func runMerge() {
    guard let inputs = config["inputs"] as? [String], !inputs.isEmpty else {
        die("merge needs a non-empty \"inputs\" array")
    }
    let output = requireText("output")
    let merged = PDFDocument()
    for path in inputs {
        let source = loadDoc(path)
        for index in 0..<source.pageCount {
            guard let page = source.page(at: index) else {
                die("merge: cannot read page \(index + 1) from \(path)")
            }
            let before = merged.pageCount
            merged.insert(page, at: before)
            guard merged.pageCount == before + 1 else {
                die("merge: cannot copy page \(index + 1) from \(path)")
            }
        }
    }
    guard merged.write(to: URL(fileURLWithPath: output)) else {
        die("merge: cannot write \(output)")
    }
    emit(["output": output, "pages": merged.pageCount])
}

func runSplit() {
    let input = requireText("input")
    let doc = loadDoc(input)
    let dir = (config["outputDir"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        ?? URL(fileURLWithPath: input).deletingLastPathComponent().path
    guard let ranges = config["ranges"] as? [[String: Any]], !ranges.isEmpty else {
        die("split needs a non-empty \"ranges\" array of {from,to}")
    }
    let base = URL(fileURLWithPath: input).deletingPathExtension().lastPathComponent
    let dirURL = URL(fileURLWithPath: dir, isDirectory: true)
    var outputs: [String] = []
    for entry in ranges {
        guard let from = entry["from"] as? Int, let to = entry["to"] as? Int else {
            die("split: every range needs integer \"from\" and \"to\"")
        }
        guard from >= 1, to >= from, to <= doc.pageCount else {
            die("split: range \(from)-\(to) is outside the 1...\(doc.pageCount) pages of \(input)")
        }
        let part = PDFDocument()
        for index in (from - 1)..<to {
            guard let page = doc.page(at: index) else {
                die("split: cannot read page \(index + 1) from \(input)")
            }
            let before = part.pageCount
            part.insert(page, at: before)
            guard part.pageCount == before + 1 else {
                die("split: cannot copy page \(index + 1) from \(input)")
            }
        }
        let url = dirURL.appendingPathComponent("\(base)-\(from)-\(to).pdf")
        guard part.write(to: url) else {
            die("split: cannot write \(url.path)")
        }
        outputs.append(url.path)
    }
    emit(["outputs": outputs, "pages": doc.pageCount])
}

// Form fields go through the PDFAnnotationUtilities category: fieldName
// matches the widget, widgetStringValue carries text values, and buttons
// (checkboxes/radios) carry a state plus the ON-state name that separates
// radio options sharing one field name.
func setField(_ doc: PDFDocument, _ name: String, _ value: String) -> Int {
    var touched = 0
    for index in 0..<doc.pageCount {
        guard let page = doc.page(at: index) else { continue }
        for annotation in page.annotations where annotation.fieldName == name {
            if annotation.widgetFieldType == .button {
                let isOn = annotation.buttonWidgetState == .onState
                let wantOn: Bool
                if value.isEmpty || value.lowercased() == "off" {
                    wantOn = false
                } else {
                    // Checkboxes report "On"; a radio widget reports its own
                    // option name and only turns on when the value matches.
                    let state = annotation.buttonWidgetStateString
                    wantOn = state.isEmpty || state == value || state.lowercased() == value.lowercased()
                }
                if isOn != wantOn {
                    annotation.buttonWidgetState = wantOn ? .onState : .offState
                    touched += 1
                }
            } else {
                annotation.widgetStringValue = value
                touched += 1
            }
        }
    }
    return touched
}

func runFill() {
    let input = requireText("input")
    let output = requireText("output")
    guard let fields = config["fields"] as? [String: String], !fields.isEmpty else {
        die("fill needs a non-empty \"fields\" object of {name: string}")
    }
    let doc = loadDoc(input)
    var filled: [String: Int] = [:]
    for (name, value) in fields {
        filled[name] = setField(doc, name, value)
    }
    guard doc.write(to: URL(fileURLWithPath: output)) else {
        die("fill: cannot write \(output)")
    }
    emit(["output": output, "pages": doc.pageCount, "filled": filled])
}

switch mode {
case "merge": runMerge()
case "split": runSplit()
case "fill": runFill()
default: die("unknown mode \"\(mode)\"")
}
"#;

fn require_pdf_ext(path: &Path, label: &str) -> std::result::Result<(), String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext != "pdf" {
        return Err(format!("\"{label}\" must end with .pdf (got \"{ext}\")"));
    }
    Ok(())
}

// (label shown back to the model, resolved existing file path).
fn resolve_input(raw: &str, ctx: &ToolCtx, wd: &Path) -> Result<(String, PathBuf)> {
    match ctx.project_id {
        Some(pid) => with_conn(ctx, |conn| fs_ops::resolve_existing(conn, wd, pid, raw))
            .map(|path| (raw.to_owned(), path)),
        None => {
            let path = resolve_path(raw, wd);
            let path = super::harness_tools::lenient_existing(&path)?;
            match fs::metadata(&path) {
                Ok(m) if m.is_file() => Ok((path.display().to_string(), path)),
                Ok(_) => Err(AppError::Invalid(format!("{} is a directory", path.display()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Err(AppError::Invalid(format!("{} not found", path.display())))
                }
                Err(e) => Err(e.into()),
            }
        }
    }
}

// (label, absolute target path): project scope joins strictly inside the
// project root (same rules as create_document), harness scope resolves
// absolute/~/working-dir-relative paths.
fn resolve_output(raw: &str, ctx: &ToolCtx, wd: &Path) -> Result<(String, PathBuf)> {
    if raw.trim().is_empty() {
        return Err(AppError::Invalid("output path is empty".into()));
    }
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            let joined = crate::path_policy::join_rel(&root, raw)?;
            let name = joined
                .file_name()
                .ok_or_else(|| AppError::Invalid(format!("invalid path \"{raw}\"")))?;
            fs_ops::check_name(&name.to_string_lossy())?;
            let joined = crate::path_policy::lenient_parent(&joined)?;
            if !canonicalish(&joined).starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok((raw.to_owned(), joined))
        }
        None => {
            let path = resolve_path(raw, wd);
            let name = path
                .file_name()
                .ok_or_else(|| AppError::Invalid(format!("invalid path \"{raw}\"")))?;
            fs_ops::check_name(&name.to_string_lossy())?;
            let path = crate::path_policy::lenient_parent(&path)?;
            Ok((path.display().to_string(), path))
        }
    }
}

fn guard_not_dir(path: &Path) -> Option<ToolResult> {
    path.is_dir().then(|| {
        ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!("{} is a directory", path.display()),
        )
    })
}

// Existing outputs are only replaced with an explicit overwrite=true.
fn guard_outputs(paths: &[PathBuf], args: &Value) -> Option<ToolResult> {
    if args.get("overwrite").and_then(Value::as_bool).unwrap_or_default() {
        return None;
    }
    let existing: Vec<String> = paths
        .iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    (!existing.is_empty()).then(|| {
        ToolResult::err(
            ToolErrorCode::InvalidArgs,
            format!(
                "{} already exist(s); pass overwrite=true to replace",
                existing.join(", ")
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Pure plan builders: validated args + resolved paths -> the swift config.
// Split also yields the derived output paths so the overwrite guard can see
// them before anything runs.
// ---------------------------------------------------------------------------

fn merge_plan(args: &Value, inputs: &[PathBuf], output: &Path) -> std::result::Result<Value, String> {
    let raw = args
        .get("inputs")
        .and_then(Value::as_array)
        .ok_or_else(|| "\"inputs\" must be an array of file paths".to_string())?;
    if raw.is_empty() {
        return Err("\"inputs\" must contain at least one PDF".to_string());
    }
    for path in inputs {
        require_pdf_ext(path, "input")?;
    }
    require_pdf_ext(output, "output")?;
    Ok(json!({
        "mode": "merge",
        "inputs": inputs.iter().map(|p| p.display().to_string()).collect::<Vec<String>>(),
        "output": output.display().to_string(),
    }))
}

// Explicit ranges or single pages (as one-page ranges), never both.
fn split_ranges(args: &Value) -> std::result::Result<Vec<(i64, i64)>, String> {
    let ranges = args.get("ranges").and_then(Value::as_array);
    let pages = args.get("pages").and_then(Value::as_array);
    match (ranges, pages) {
        (Some(_), Some(_)) => Err("provide either ranges [{from,to}] or pages [n], not both".to_string()),
        (None, None) => Err("either ranges [{from,to}] or pages [n] is required".to_string()),
        (Some(entries), None) => {
            if entries.is_empty() {
                return Err("\"ranges\" must contain at least one {from,to}".to_string());
            }
            let mut out = Vec::with_capacity(entries.len());
            for entry in entries {
                let from = entry
                    .get("from")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "every range needs an integer \"from\"".to_string())?;
                let to = entry
                    .get("to")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "every range needs an integer \"to\"".to_string())?;
                if from < 1 {
                    return Err(format!("range.from must be 1-based, got {from}"));
                }
                if to < from {
                    return Err(format!("range.to ({to}) must be >= range.from ({from})"));
                }
                out.push((from, to));
            }
            Ok(out)
        }
        (None, Some(entries)) => {
            if entries.is_empty() {
                return Err("\"pages\" must contain at least one page number".to_string());
            }
            let mut out = Vec::with_capacity(entries.len());
            for entry in entries {
                let page = entry
                    .as_i64()
                    .ok_or_else(|| "\"pages\" entries must be integers".to_string())?;
                if page < 1 {
                    return Err(format!("page numbers are 1-based, got {page}"));
                }
                out.push((page, page));
            }
            Ok(out)
        }
    }
}

fn split_plan(
    args: &Value,
    input: &Path,
    output_dir: &Path,
) -> std::result::Result<(Value, Vec<PathBuf>), String> {
    require_pdf_ext(input, "input")?;
    let ranges = split_ranges(args)?;
    let base = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_owned());
    let outputs = ranges
        .iter()
        .map(|(from, to)| output_dir.join(format!("{base}-{from}-{to}.pdf")))
        .collect();
    let config = json!({
        "mode": "split",
        "input": input.display().to_string(),
        "outputDir": output_dir.display().to_string(),
        "ranges": ranges.iter().map(|(from, to)| json!({"from": from, "to": to})).collect::<Vec<Value>>(),
    });
    Ok((config, outputs))
}

fn fill_plan(args: &Value, input: &Path, output: &Path) -> std::result::Result<Value, String> {
    require_pdf_ext(input, "input")?;
    require_pdf_ext(output, "output")?;
    let fields = args
        .get("fields")
        .and_then(Value::as_object)
        .ok_or_else(|| "\"fields\" must be an object of {name: string}".to_string())?;
    if fields.is_empty() {
        return Err("\"fields\" must contain at least one {name: value} entry".to_string());
    }
    for (name, value) in fields {
        if name.trim().is_empty() {
            return Err("field names must not be empty".to_string());
        }
        if value.as_str().is_none() {
            return Err(format!("field \"{name}\" value must be a string"));
        }
    }
    Ok(json!({
        "mode": "fill",
        "input": input.display().to_string(),
        "output": output.display().to_string(),
        "fields": fields,
    }))
}

// ---------------------------------------------------------------------------
// Swift helper plumbing (same shape as ocr_tools.rs)
// ---------------------------------------------------------------------------

fn read_capped(mut reader: impl Read, cap: usize) -> String {
    let mut buf = Vec::new();
    let _ = (&mut reader).take((cap as u64) + 1).read_to_end(&mut buf);
    buf.truncate(cap);
    String::from_utf8_lossy(&buf).into_owned()
}

fn prepare_script(wd: &Path) -> Result<(PathBuf, PathBuf)> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let script = tmp.join("pdf_tools.swift");
    if fs::read_to_string(&script).unwrap_or_default() != PDF_SWIFT {
        fs::write(&script, PDF_SWIFT)?;
    }
    Ok((tmp, script))
}

fn swift_or_err() -> std::result::Result<PathBuf, ToolResult> {
    super::ocr_tools::find_swift().ok_or_else(|| {
        ToolResult::err(
            ToolErrorCode::ExecutionError,
            "swift toolchain not found (checked $PROJECT_S_SWIFT, `which swift`, `xcrun --find swift`); PDF tools need the Xcode Command Line Tools — install them with `xcode-select --install`",
        )
    })
}

// Runs the helper with the config JSON on stdin; returns the parsed stdout
// JSON or a single-line failure detail for the envelope.
fn run_pdf_swift(swift: &Path, script: &Path, config: &Value, tmp: &Path) -> std::result::Result<Value, String> {
    let mut child = match Command::new(swift)
        .arg(script)
        .env("TMPDIR", tmp)
        .env("TEMP", tmp)
        .env("TMP", tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("cannot run {}: {e}", swift.display())),
    };
    let payload = config.to_string();
    let stdin_thread = child.stdin.take().map(|mut stdin| {
        std::thread::spawn(move || -> std::result::Result<(), String> {
            stdin.write_all(payload.as_bytes()).map_err(|e| e.to_string())
        })
    });
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, STDOUT_CAP)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, STDERR_CAP)));
    let deadline = Instant::now() + PDF_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed to wait for the swift helper: {e}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "pdf operation timed out after {} s and was killed",
                PDF_TIMEOUT.as_secs()
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
        return Err(format!(
            "pdf helper failed (exit {}): {detail}",
            status.code().unwrap_or(-1)
        ));
    }
    if let Some(thread) = stdin_thread {
        if let Err(e) = thread.join().unwrap_or_else(|_| Err("stdin writer panicked".into())) {
            return Err(format!("cannot send config to the swift helper: {e}"));
        }
    }
    serde_json::from_str(&stdout)
        .map_err(|_| format!("pdf helper returned invalid JSON: {}", stdout.trim()))
}

// Confirmation checks: unresolvable targets confirm, writes outside the
// harness territory confirm, and so does every call that replaces an
// existing file; a fresh file inside the territory does not.
fn confirm_for(
    tool: &str,
    outputs: std::result::Result<Vec<PathBuf>, AppError>,
    ctx: &ToolCtx,
    wd: &Path,
) -> Option<String> {
    let outputs = match outputs {
        Ok(paths) => paths,
        Err(e) => return Some(format!("{tool}: {}", e.to_string().replace("invalid input: ", ""))),
    };
    for path in outputs {
        if ctx.project_id.is_none() && !in_default_territory(&path, wd) {
            return Some(format!(
                "запись результата {tool} вне Harness/ и Projects/ ({})",
                canonicalish(&path).display()
            ));
        }
        if path.exists() {
            return Some(format!("{tool} перезаписывает существующий файл {}", path.display()));
        }
    }
    None
}

fn merge_outputs(args: &Value, ctx: &ToolCtx, wd: &Path) -> std::result::Result<Vec<PathBuf>, AppError> {
    Ok(vec![resolve_output(args.get("output").and_then(Value::as_str).unwrap_or_default(), ctx, wd)?.1])
}

fn split_outputs(args: &Value, ctx: &ToolCtx, wd: &Path) -> std::result::Result<Vec<PathBuf>, AppError> {
    let (_, input) = resolve_input(args.get("input").and_then(Value::as_str).unwrap_or_default(), ctx, wd)?;
    let dir = match args.get("outputDir").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(raw) => resolve_output(raw, ctx, wd)?.1,
        None => input.parent().map(Path::to_path_buf).unwrap_or_else(|| wd.to_path_buf()),
    };
    let ranges = split_ranges(args).map_err(AppError::Invalid)?;
    let base = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "part".to_owned());
    Ok(ranges
        .iter()
        .map(|(from, to)| dir.join(format!("{base}-{from}-{to}.pdf")))
        .collect())
}

fn fill_outputs(args: &Value, ctx: &ToolCtx, wd: &Path) -> std::result::Result<Vec<PathBuf>, AppError> {
    Ok(vec![resolve_output(args.get("output").and_then(Value::as_str).unwrap_or_default(), ctx, wd)?.1])
}

// ---------------------------------------------------------------------------
// pdf_merge
// ---------------------------------------------------------------------------

pub struct PdfMergeTool;

impl Tool for PdfMergeTool {
    fn name(&self) -> &str {
        "pdf_merge"
    }
    fn description(&self) -> &str {
        "Merge PDF files into one with Apple PDFKit through a Swift helper (requires the Xcode Command Line Tools); pages are appended in the order given by inputs. All paths must end with .pdf and are project-relative (project agent) or absolute/~/working-dir-relative (harness agent). Existing output is only replaced with overwrite=true. Returns {output, pages}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "inputs": { "type": "array", "items": { "type": "string" } },
                "output": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["inputs", "output"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let wd = ctx.working_dir.as_deref()?;
        confirm_for("pdf_merge", merge_outputs(args, ctx, wd), ctx, wd)
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let raw_inputs = args.get("inputs").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut inputs: Vec<PathBuf> = Vec::new();
        for raw in &raw_inputs {
            let Some(raw) = raw.as_str() else {
                return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "\"inputs\" entries must be strings"));
            };
            match resolve_input(raw, ctx, &wd) {
                Ok((_, path)) => inputs.push(path),
                Err(e) => return Ok(envelope::<()>(Err(e))),
            }
        }
        let (label, output) = match resolve_output(args.get("output").and_then(Value::as_str).unwrap_or_default(), ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let config = match merge_plan(args, &inputs, &output) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        if let Some(r) = guard_not_dir(&output).or_else(|| guard_outputs(std::slice::from_ref(&output), args)) {
            return Ok(r);
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let (tmp, script) = prepare_script(&wd)?;
        let swift = match swift_or_err() {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        match run_pdf_swift(&swift, &script, &config, &tmp) {
            Ok(data) => Ok(ToolResult::ok(json!({ "output": label, "pages": data["pages"] }))),
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        }
    }
}

// ---------------------------------------------------------------------------
// pdf_split
// ---------------------------------------------------------------------------

pub struct PdfSplitTool;

impl Tool for PdfSplitTool {
    fn name(&self) -> &str {
        "pdf_split"
    }
    fn description(&self) -> &str {
        "Split a PDF into separate files with Apple PDFKit through a Swift helper (requires the Xcode Command Line Tools). Provide either ranges [{from,to}] (1-based inclusive page ranges) or pages [n] (single pages); each part is written as <base>-<from>-<to>.pdf into outputDir (default: the input's folder). Existing parts are only replaced with overwrite=true. Paths are project-relative (project agent) or absolute/~/working-dir-relative (harness agent). Returns {outputs, pages}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": { "type": "string" },
                "ranges": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "from": { "type": "integer" },
                            "to": { "type": "integer" }
                        },
                        "required": ["from", "to"],
                        "additionalProperties": false
                    }
                },
                "pages": { "type": "array", "items": { "type": "integer" } },
                "outputDir": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["input"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let wd = ctx.working_dir.as_deref()?;
        confirm_for("pdf_split", split_outputs(args, ctx, wd), ctx, wd)
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let (in_label, input) = match resolve_input(args.get("input").and_then(Value::as_str).unwrap_or_default(), ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let (dir_label, output_dir) = match args.get("outputDir").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(raw) => match resolve_output(raw, ctx, &wd) {
                Ok(v) => v,
                Err(e) => return Ok(envelope::<()>(Err(e))),
            },
            None => {
                let parent = input.parent().map(Path::to_path_buf).unwrap_or_else(|| wd.clone());
                let label = Path::new(&in_label)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                (label, parent)
            }
        };
        let (config, outputs) = match split_plan(args, &input, &output_dir) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        let dir_is_file = output_dir
            .exists()
            .then(|| fs::metadata(&output_dir).ok().filter(|m| !m.is_dir()))
            .flatten()
            .map(|_| {
                ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} is not a directory", output_dir.display()),
                )
            });
        if let Some(r) = dir_is_file.or_else(|| guard_outputs(&outputs, args)) {
            return Ok(r);
        }
        fs::create_dir_all(&output_dir)?;
        let (tmp, script) = prepare_script(&wd)?;
        let swift = match swift_or_err() {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        match run_pdf_swift(&swift, &script, &config, &tmp) {
            Ok(data) => {
                let labels: Vec<String> = outputs
                    .iter()
                    .map(|p| Path::new(&dir_label).join(p.file_name().unwrap_or_default()).display().to_string())
                    .collect();
                Ok(ToolResult::ok(json!({ "outputs": labels, "pages": data["pages"] })))
            }
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        }
    }
}

// ---------------------------------------------------------------------------
// pdf_fill_form
// ---------------------------------------------------------------------------

pub struct PdfFillFormTool;

impl Tool for PdfFillFormTool {
    fn name(&self) -> &str {
        "pdf_fill_form"
    }
    fn description(&self) -> &str {
        "Fill AcroForm fields of a PDF with Apple PDFKit through a Swift helper (requires the Xcode Command Line Tools). fields maps field name to value: text fields get the string value, checkboxes and radio buttons take \"on\"/\"off\" or the radio option name. All paths must end with .pdf and are project-relative (project agent) or absolute/~/working-dir-relative (harness agent). Existing output is only replaced with overwrite=true. Returns {output, pages, filled} where filled counts the widgets matched per field name."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": { "type": "string" },
                "output": { "type": "string" },
                "fields": { "type": "object", "additionalProperties": { "type": "string" } },
                "overwrite": { "type": "boolean" }
            },
            "required": ["input", "output", "fields"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let wd = ctx.working_dir.as_deref()?;
        confirm_for("pdf_fill_form", fill_outputs(args, ctx, wd), ctx, wd)
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let (_, input) = match resolve_input(args.get("input").and_then(Value::as_str).unwrap_or_default(), ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let (label, output) = match resolve_output(args.get("output").and_then(Value::as_str).unwrap_or_default(), ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let config = match fill_plan(args, &input, &output) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };
        if let Some(r) = guard_not_dir(&output).or_else(|| guard_outputs(std::slice::from_ref(&output), args)) {
            return Ok(r);
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let (tmp, script) = prepare_script(&wd)?;
        let swift = match swift_or_err() {
            Ok(p) => p,
            Err(r) => return Ok(r),
        };
        match run_pdf_swift(&swift, &script, &config, &tmp) {
            Ok(data) => Ok(ToolResult::ok(json!({
                "output": label,
                "pages": data["pages"],
                "filled": data["filled"],
            }))),
            Err(e) => Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        }
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
        for tool in [
            Box::new(PdfMergeTool) as Box<dyn Tool>,
            Box::new(PdfSplitTool),
            Box::new(PdfFillFormTool),
        ] {
            assert_eq!(tool.class(), ToolClass::Mutating, "{}", tool.name());
            assert_eq!(tool.policy(), PermissionPolicy::CheckConditional, "{}", tool.name());
            assert_eq!(tool.parameters()["additionalProperties"], false, "{}", tool.name());
        }
        let merge = PdfMergeTool.parameters();
        assert!(validate_args(&merge, &json!({ "inputs": ["a.pdf"], "output": "b.pdf" })).is_ok());
        assert!(validate_args(&merge, &json!({ "inputs": [], "output": "b.pdf", "overwrite": true })).is_ok());
        assert!(validate_args(&merge, &json!({ "inputs": "a.pdf", "output": "b.pdf" }))
            .unwrap_err()
            .contains("array"));
        assert!(validate_args(&merge, &json!({ "inputs": ["a.pdf"] })).unwrap_err().contains("output"));
        assert!(validate_args(&merge, &json!({ "inputs": ["a.pdf"], "output": "b.pdf", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));

        let split = PdfSplitTool.parameters();
        assert!(validate_args(&split, &json!({ "input": "a.pdf", "ranges": [{ "from": 1, "to": 2 }] })).is_ok());
        assert!(validate_args(&split, &json!({ "input": "a.pdf", "pages": [1, 2], "outputDir": "out" })).is_ok());
        assert!(validate_args(&split, &json!({ "input": "a.pdf" })).is_ok());
        assert!(validate_args(&split, &json!({ "input": "a.pdf", "ranges": [{ "from": 1 }] }))
            .unwrap_err()
            .contains("to"));
        assert!(validate_args(&split, &json!({ "input": "a.pdf", "ranges": [{ "from": 1, "to": 2, "extra": 1 }] }))
            .unwrap_err()
            .contains("extra"));
        assert!(validate_args(&split, &json!({ "input": "a.pdf", "pages": ["x"] }))
            .unwrap_err()
            .contains("integer"));

        let fill = PdfFillFormTool.parameters();
        assert!(validate_args(&fill, &json!({ "input": "a.pdf", "output": "b.pdf", "fields": { "name": "x", "agree": "on" } })).is_ok());
        assert!(validate_args(&fill, &json!({ "input": "a.pdf", "output": "b.pdf" })).unwrap_err().contains("fields"));
        assert!(validate_args(&fill, &json!({ "input": "a.pdf", "output": "b.pdf", "fields": { "n": 5 } }))
            .unwrap_err()
            .contains("string"));
    }

    #[test]
    fn merge_plan_builds_config_and_rejects_bad_args() {
        let inputs = vec![PathBuf::from("/x/a.pdf"), PathBuf::from("/x/b.pdf")];
        let config = merge_plan(&json!({ "inputs": ["a.pdf", "b.pdf"] }), &inputs, Path::new("/x/o.pdf")).unwrap();
        assert_eq!(config["mode"], "merge");
        assert_eq!(config["inputs"][0], "/x/a.pdf");
        assert_eq!(config["output"], "/x/o.pdf");

        assert!(merge_plan(&json!({ "inputs": [] }), &[], Path::new("/o.pdf"))
            .unwrap_err()
            .contains("at least one"));
        assert!(merge_plan(&json!({ "inputs": ["a.txt"] }), &[PathBuf::from("/a.txt")], Path::new("/o.pdf"))
            .unwrap_err()
            .contains(".pdf"));
        assert!(merge_plan(&json!({ "inputs": ["a.pdf"] }), &[PathBuf::from("/a.pdf")], Path::new("/o.txt"))
            .unwrap_err()
            .contains(".pdf"));
    }

    #[test]
    fn split_plan_validates_ranges_pages_and_derives_names() {
        let input = PathBuf::from("/x/report.pdf");
        let dir = PathBuf::from("/x/out");
        let (config, outs) =
            split_plan(&json!({ "ranges": [{ "from": 1, "to": 2 }, { "from": 3, "to": 3 }] }), &input, &dir).unwrap();
        assert_eq!(config["mode"], "split");
        assert_eq!(config["ranges"][0]["from"], 1);
        assert_eq!(outs, vec![
            PathBuf::from("/x/out/report-1-2.pdf"),
            PathBuf::from("/x/out/report-3-3.pdf"),
        ]);
        let (_, outs) = split_plan(&json!({ "pages": [2, 1] }), &input, &dir).unwrap();
        assert_eq!(outs, vec![
            PathBuf::from("/x/out/report-2-2.pdf"),
            PathBuf::from("/x/out/report-1-1.pdf"),
        ]);

        assert!(split_plan(&json!({}), &input, &dir).unwrap_err().contains("required"));
        assert!(split_plan(&json!({ "ranges": [{ "from": 1, "to": 2 }], "pages": [1] }), &input, &dir)
            .unwrap_err()
            .contains("not both"));
        assert!(split_plan(&json!({ "ranges": [] }), &input, &dir).unwrap_err().contains("at least one"));
        assert!(split_plan(&json!({ "pages": [] }), &input, &dir).unwrap_err().contains("at least one"));
        assert!(split_plan(&json!({ "ranges": [{ "from": 0, "to": 1 }] }), &input, &dir)
            .unwrap_err()
            .contains("1-based"));
        assert!(split_plan(&json!({ "ranges": [{ "from": 2, "to": 1 }] }), &input, &dir)
            .unwrap_err()
            .contains(">= range.from"));
        assert!(split_plan(&json!({ "pages": [0] }), &input, &dir).unwrap_err().contains("1-based"));
        assert!(split_plan(&json!({ "pages": [1] }), &PathBuf::from("/x/a.txt"), &dir)
            .unwrap_err()
            .contains(".pdf"));
    }

    #[test]
    fn fill_plan_requires_nonempty_string_fields() {
        let config = fill_plan(
            &json!({ "fields": { "name": "Ivan", "agree": "on" } }),
            Path::new("/a.pdf"),
            Path::new("/b.pdf"),
        )
        .unwrap();
        assert_eq!(config["mode"], "fill");
        assert_eq!(config["fields"]["agree"], "on");

        assert!(fill_plan(&json!({ "fields": {} }), Path::new("/a.pdf"), Path::new("/b.pdf"))
            .unwrap_err()
            .contains("at least one"));
        assert!(fill_plan(&json!({ "fields": { "": "x" } }), Path::new("/a.pdf"), Path::new("/b.pdf"))
            .unwrap_err()
            .contains("empty"));
        assert!(fill_plan(&json!({}), Path::new("/a.pdf"), Path::new("/b.pdf"))
            .unwrap_err()
            .contains("object"));
        assert!(fill_plan(&json!({ "fields": { "n": 5 } }), Path::new("/a.pdf"), Path::new("/b.pdf"))
            .unwrap_err()
            .contains("string"));
        assert!(fill_plan(&json!({ "fields": { "n": "x" } }), Path::new("/a.txt"), Path::new("/b.pdf"))
            .unwrap_err()
            .contains(".pdf"));
    }

    #[test]
    fn project_paths_stay_inside_and_missing_inputs_are_not_found() {
        let (root, ctx) = project_env("pdf-escape");
        fs::write(root.join("a.pdf"), b"%PDF-1.4\n").unwrap();

        let r = PdfMergeTool
            .execute(&json!({ "inputs": ["../x.pdf"], "output": "out.pdf" }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "permission_denied");
        for bad in ["../out.pdf", "/tmp/out.pdf", "a/../../out.pdf"] {
            let r = PdfMergeTool
                .execute(&json!({ "inputs": ["a.pdf"], "output": bad }), &ctx)
                .unwrap();
            assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
        }
        let r = PdfSplitTool
            .execute(&json!({ "input": "missing.pdf", "pages": [1] }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = PdfFillFormTool
            .execute(&json!({ "input": "a.pdf", "output": "../out.pdf", "fields": { "n": "x" } }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "permission_denied");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn overwrite_guard_requires_explicit_flag() {
        let (root, ctx) = project_env("pdf-overwrite");
        fs::write(root.join("a.pdf"), b"%PDF-1.4\n").unwrap();
        fs::write(root.join("b.pdf"), b"%PDF-1.4\n").unwrap();

        let r = PdfMergeTool
            .execute(&json!({ "inputs": ["a.pdf"], "output": "b.pdf" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("overwrite=true"), "{}", err.message);
        // With the flag the guard passes; the run may still fail later (e.g.
        // no swift toolchain or the dummy input is not a real PDF), but the
        // overwrite error must be gone.
        let r = PdfMergeTool
            .execute(&json!({ "inputs": ["a.pdf"], "output": "b.pdf", "overwrite": true }), &ctx)
            .unwrap();
        if let Some(err) = r.error {
            assert!(!err.message.contains("overwrite=true"), "{}", err.message);
        }

        fs::write(root.join("a-1-1.pdf"), b"%PDF-1.4\n").unwrap();
        let r = PdfSplitTool
            .execute(&json!({ "input": "a.pdf", "pages": [1] }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("a-1-1.pdf"), "{}", err.message);
        assert!(err.message.contains("overwrite=true"), "{}", err.message);

        fs::create_dir_all(root.join("out.pdf")).unwrap();
        let r = PdfMergeTool
            .execute(&json!({ "inputs": ["a.pdf"], "output": "out.pdf" }), &ctx)
            .unwrap();
        assert!(r.error.unwrap().message.contains("is a directory"));
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn danger_reason_confirms_overwrites_escapes_and_outside_writes() {
        let (root, ctx) = project_env("pdf-policy");
        fs::write(root.join("a.pdf"), b"%PDF-1.4\n").unwrap();
        let t = PdfMergeTool;
        assert_eq!(
            t.danger_reason(&json!({ "inputs": ["a.pdf"], "output": "new.pdf" }), &ctx),
            None,
            "new file inside the project must not confirm"
        );
        fs::write(root.join("b.pdf"), b"x").unwrap();
        let reason = t.danger_reason(&json!({ "inputs": ["a.pdf"], "output": "b.pdf" }), &ctx);
        assert!(reason.as_deref().unwrap().contains("перезаписывает"), "{reason:?}");
        let escape = t.danger_reason(&json!({ "inputs": ["a.pdf"], "output": "../b.pdf" }), &ctx);
        assert!(escape.is_some(), "escaping output must confirm");
        let split = PdfSplitTool.danger_reason(&json!({ "input": "a.pdf", "pages": [1] }), &ctx);
        assert_eq!(split, None, "fresh split parts inside the project must not confirm");
        fs::write(root.join("a-1-1.pdf"), b"x").unwrap();
        let split = PdfSplitTool.danger_reason(&json!({ "input": "a.pdf", "pages": [1] }), &ctx);
        assert!(split.as_deref().unwrap().contains("a-1-1.pdf"), "{split:?}");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();

        let (wd, ctx) = harness_env("pdf-policy-h");
        fs::write(wd.join("Harness/a.pdf"), b"%PDF-1.4\n").unwrap();
        assert_eq!(
            t.danger_reason(&json!({ "inputs": ["Harness/a.pdf"], "output": "Harness/out.pdf" }), &ctx),
            None,
            "new file inside Harness/ must not confirm"
        );
        let outside = t.danger_reason(&json!({ "inputs": ["Harness/a.pdf"], "output": "outside.pdf" }), &ctx);
        assert!(outside.as_deref().unwrap().contains("Harness/"), "{outside:?}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn helper_script_is_written_under_harness_tmp_and_rewritten_when_stale() {
        let (wd, ctx) = harness_env("pdf-script");
        fs::write(wd.join("Harness/a.pdf"), b"%PDF-1.4\n").unwrap();
        // The run itself may fail on a machine without the swift toolchain;
        // the helper script must exist under Harness/tmp either way.
        let _ = PdfMergeTool.execute(&json!({ "inputs": ["Harness/a.pdf"], "output": "Harness/out.pdf" }), &ctx);
        let script = paths::harness_tmp_dir(&wd).join("pdf_tools.swift");
        assert_eq!(fs::read_to_string(&script).unwrap(), PDF_SWIFT);

        fs::write(&script, "stale").unwrap();
        let _ = PdfMergeTool.execute(&json!({ "inputs": ["Harness/a.pdf"], "output": "Harness/out.pdf" }), &ctx);
        assert_eq!(fs::read_to_string(&script).unwrap(), PDF_SWIFT);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_swift_toolchain_reports_clear_error() {
        let (wd, ctx) = harness_env("pdf-noswift");
        fs::write(wd.join("Harness/a.pdf"), b"%PDF-1.4\n").unwrap();
        std::env::set_var("PROJECT_S_SWIFT", wd.join("no-such-swift"));
        let r = PdfMergeTool
            .execute(&json!({ "inputs": ["Harness/a.pdf"], "output": "Harness/out.pdf" }), &ctx)
            .unwrap();
        std::env::remove_var("PROJECT_S_SWIFT");
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("Xcode Command Line Tools"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    #[ignore = "runs the real swift+PDFKit toolchain end to end"]
    fn live_merge_and_split_round_trip() {
        let Some(swift) = crate::agent::ocr_tools::find_swift() else {
            eprintln!("no swift toolchain on this machine, skipping live PDF merge/split test");
            return;
        };
        let (root, ctx) = project_env("pdf-live");
        for (name, text) in [("part-a.pdf", "Page A of the merge test."), ("part-b.pdf", "Page B of the merge test.")] {
            let r = CreateDocumentTool::project()
                .execute(&json!({ "path": name, "format": "pdf", "title": name, "markdown": text }), &ctx)
                .unwrap();
            assert!(r.ok, "create_document failed: {:?}", r.error);
        }

        let r = PdfMergeTool
            .execute(&json!({ "inputs": ["part-a.pdf", "part-b.pdf"], "output": "merged.pdf" }), &ctx)
            .unwrap();
        assert!(r.ok, "swift {}: {:?}", swift.display(), r.error);
        let data = r.data.unwrap();
        assert_eq!(data["output"], "merged.pdf");
        assert_eq!(data["pages"].as_u64().unwrap(), 2, "merged page count must be the sum");
        assert_eq!(&fs::read(root.join("merged.pdf")).unwrap()[..4], b"%PDF");

        let r = PdfSplitTool
            .execute(
                &json!({ "input": "merged.pdf", "ranges": [{ "from": 1, "to": 1 }, { "from": 2, "to": 2 }] }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["pages"].as_u64().unwrap(), 2);
        let outputs = data["outputs"].as_array().unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], "merged-1-1.pdf");
        assert_eq!(outputs[1], "merged-2-2.pdf");
        for name in ["merged-1-1.pdf", "merged-2-2.pdf"] {
            assert_eq!(&fs::read(root.join(name)).unwrap()[..4], b"%PDF", "{name}");
        }

        // single-page split into a subfolder
        let r = PdfSplitTool
            .execute(&json!({ "input": "merged.pdf", "pages": [2], "outputDir": "parts" }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["outputs"][0], "parts/merged-2-2.pdf");
        assert_eq!(&fs::read(root.join("parts/merged-2-2.pdf")).unwrap()[..4], b"%PDF");

        // out-of-range split fails inside the helper with a clear message
        let r = PdfSplitTool
            .execute(&json!({ "input": "merged.pdf", "ranges": [{ "from": 1, "to": 9 }] }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("outside the 1...2 pages"), "{}", err.message);
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    // A minimal hand-crafted AcroForm PDF: one page with a text field
    // "name", a checkbox "agree" (Off/On appearances) and a radio group
    // "color" (Red/Blue widgets). Object offsets are recorded while the
    // file is assembled, so the xref table stays valid.
    fn form_pdf_fixture() -> Vec<u8> {
        let form_xobject = || {
            "<< /Type /XObject /Subtype /Form /BBox [0 0 12 12] /Length 0 >>\nstream\nendstream".to_string()
        };
        let page_body = "BT /F1 12 Tf 10 60 Td (form fixture) Tj ET\n";
        let objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [4 0 R 5 0 R 6 0 R] /NeedAppearances true >> >>".into(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 13 0 R /Resources << /Font << /F1 14 0 R >> >> /Annots [4 0 R 5 0 R 7 0 R 8 0 R] >>".into(),
            "<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V () /Rect [10 150 190 170] /DA (/F1 12 Tf 0 g) >>".into(),
            "<< /Type /Annot /Subtype /Widget /FT /Btn /T (agree) /Rect [10 120 22 132] /AP << /N << /Off 9 0 R /On 10 0 R >> >> /AS /Off >>".into(),
            "<< /Type /Annot /Subtype /Widget /FT /Btn /Ff 32768 /T (color) /Kids [7 0 R 8 0 R] >>".into(),
            "<< /Type /Annot /Subtype /Widget /Parent 6 0 R /Rect [10 100 22 112] /AP << /N << /Red 11 0 R >> >> /AS /Off >>".into(),
            "<< /Type /Annot /Subtype /Widget /Parent 6 0 R /Rect [30 100 42 112] /AP << /N << /Blue 12 0 R >> >> /AS /Off >>".into(),
            form_xobject(),
            form_xobject(),
            form_xobject(),
            form_xobject(),
            format!("<< /Length {} >>\nstream\n{page_body}endstream", page_body.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, obj) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{obj}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for off in &offsets {
            out.extend(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend(
            format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1)
                .as_bytes(),
        );
        out
    }

    #[test]
    #[ignore = "runs the real swift+PDFKit toolchain end to end"]
    fn live_fill_form_sets_text_and_button_states() {
        let Some(swift) = crate::agent::ocr_tools::find_swift() else {
            eprintln!("no swift toolchain on this machine, skipping live PDF fill test");
            return;
        };
        let (root, ctx) = project_env("pdf-fill-live");
        fs::write(root.join("form.pdf"), form_pdf_fixture()).unwrap();

        let r = PdfFillFormTool
            .execute(
                &json!({
                    "input": "form.pdf",
                    "output": "filled.pdf",
                    "fields": { "name": "Ivan Petrov", "agree": "on", "color": "Blue" },
                }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "swift {}: {:?}", swift.display(), r.error);
        let data = r.data.unwrap();
        assert_eq!(data["output"], "filled.pdf");
        assert_eq!(data["pages"].as_u64().unwrap(), 1);
        assert_eq!(data["filled"]["name"].as_u64().unwrap(), 1);
        assert_eq!(data["filled"]["agree"].as_u64().unwrap(), 1);
        assert_eq!(data["filled"]["color"].as_u64().unwrap(), 1);

        // Probe the written file with a throwaway swift snippet: the field
        // values (/V, surfaced as widgetStringValue) must have survived the
        // save — the checkbox carries its on-state name and the radio group
        // carries the chosen option name.
        let tmp = paths::harness_tmp_dir(&ctx.working_dir.clone().unwrap());
        let probe_script = tmp.join("pdf_probe.swift");
        fs::write(
            &probe_script,
            r#"import Foundation
import PDFKit

guard let doc = PDFDocument(url: URL(fileURLWithPath: CommandLine.arguments[1])) else {
    print("cannot open")
    exit(1)
}
for index in 0..<doc.pageCount {
    guard let page = doc.page(at: index) else { continue }
    for a in page.annotations {
        print("\(a.fieldName ?? "?")|\(a.widgetStringValue ?? "")")
    }
}
"#,
        )
        .unwrap();
        let probe = Command::new(&swift)
            .arg(&probe_script)
            .arg(root.join("filled.pdf"))
            .env("TMPDIR", &tmp)
            .output()
            .unwrap();
        assert!(probe.status.success(), "probe: {}", String::from_utf8_lossy(&probe.stderr));
        let text = String::from_utf8_lossy(&probe.stdout).into_owned();
        eprintln!("probe output:\n{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.contains(&"name|Ivan Petrov"), "{text}");
        assert!(lines.contains(&"agree|On"), "checkbox must hold its on-state: {text}");
        assert!(lines.contains(&"color|Blue"), "radio group must hold the chosen option: {text}");
        assert!(!lines.iter().any(|l| l.starts_with("color|") && *l != "color|Blue"), "radio value leaked: {text}");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }
}
