// Image transform via the macOS `sips` CLI: resize (--resampleWidth/
// --resampleHeight), thumbnail (-Z), crop (-c), rotate (-r), format convert
// (-s format) and JPEG quality (-s formatOptions). Mutating/CheckConditional:
// an in-place call (no output) always confirms because it overwrites the
// original; a new output inside Harness/ and Projects/ skips confirmation,
// anywhere else it confirms. path is project-relative (project agent) or
// absolute/~/working-dir-relative (harness agent); output follows the same
// scope. The sips child sees TMPDIR/TEMP/TMP pointed at Harness/tmp/, runs
// under a 60 s timeout, and its output must exist and be non-empty; the
// result queries the output size back with `sips -g pixelWidth -g pixelHeight`.
use super::harness_tools::{canonicalish, in_default_territory, resolve_path};
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::{fs_ops, paths, projects};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SIPS_BINARY: &str = "/usr/bin/sips";
const SIPS_TIMEOUT: Duration = Duration::from_secs(60);
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const FORMATS: [&str; 4] = ["jpeg", "png", "tiff", "heic"];
// Extensions sips can read as input (sips itself is strict about formats).
const INPUT_EXTS: [&str; 10] = [
    "png", "jpg", "jpeg", "gif", "bmp", "tiff", "tif", "heic", "heif", "jpeg2000",
];

// (label, existing input, optional output): the project-relative path as
// given in project scope, the resolved absolute path in harness scope.
fn resolve_io(args: &Value, ctx: &ToolCtx, wd: &Path) -> std::result::Result<(String, PathBuf, Option<PathBuf>), AppError> {
    let raw = args["path"].as_str().unwrap_or_default();
    let out_raw = args
        .get("output")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match ctx.project_id {
        Some(pid) => {
            let path = with_conn(ctx, |conn| fs_ops::resolve_existing(conn, wd, pid, raw))?;
            let output = match out_raw {
                Some(o) => {
                    let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
                    Some(fs_ops::lenient_parent(&fs_ops::join_rel(&root, o)?)?)
                }
                None => None,
            };
            Ok((raw.to_owned(), path, output))
        }
        None => {
            let path = super::harness_tools::lenient_existing(&resolve_path(raw, wd))?;
            let output = match out_raw {
                Some(o) => Some(fs_ops::lenient_parent(&resolve_path(o, wd))?),
                None => None,
            };
            // The label stays the path as given (project scope echoes the
            // project-relative path too); sips gets the resolved absolute one.
            Ok((raw.to_owned(), path, output))
        }
    }
}

// The validated operation set plus the resolved endpoints, turned into the
// exact sips argv. Pure so command construction is testable without sips.
fn sips_plan(
    args: &Value,
    in_path: &Path,
    out_path: Option<&Path>,
) -> std::result::Result<Vec<String>, String> {
    let pos = |key: &str| -> std::result::Result<Option<i64>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => match v.as_i64() {
                Some(n) if n > 0 => Ok(Some(n)),
                _ => Err(format!("{key} must be a positive integer")),
            },
        }
    };
    let width = pos("width")?;
    let height = pos("height")?;
    let max_size = pos("maxSize")?;
    let crop = match args.get("crop") {
        None | Some(Value::Null) => None,
        Some(c) => {
            let w = c
                .get("width")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or_else(|| "crop.width must be a positive integer".to_string())?;
            let h = c
                .get("height")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or_else(|| "crop.height must be a positive integer".to_string())?;
            Some((w, h))
        }
    };
    let rotate = match args.get("rotate") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .ok_or_else(|| "rotate must be an integer number of degrees".to_string())?,
        ),
    };
    let format = match args.get("format") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .filter(|f| FORMATS.contains(f))
                .ok_or_else(|| format!("format must be one of {}", FORMATS.join(", ")))?,
        ),
    };
    let quality = match args.get("quality") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(|| "quality must be an integer between 1 and 100".to_string())?,
        ),
    };
    // sips applies formatOptions as JPEG quality only; a png/tiff/heic output
    // would silently ignore it, so require jpeg. Checked before the
    // at-least-one-operation rule so quality-only calls get the precise hint.
    if quality.is_some() && !matches!(format, Some("jpeg")) {
        return Err("quality applies only to jpeg output; set format=\"jpeg\"".to_string());
    }
    if width.is_none()
        && height.is_none()
        && max_size.is_none()
        && crop.is_none()
        && rotate.is_none()
        && format.is_none()
    {
        return Err(
            "at least one operation is required: width, height, maxSize, crop, rotate or format"
                .to_string(),
        );
    }

    let mut argv: Vec<String> = vec![in_path.display().to_string()];
    if let Some(deg) = rotate {
        argv.extend(["-r".to_string(), deg.to_string()]);
    }
    if let Some((cw, ch)) = crop {
        // sips takes -c <height> <width>.
        argv.extend(["-c".to_string(), ch.to_string(), cw.to_string()]);
    }
    if let Some(m) = max_size {
        argv.extend(["-Z".to_string(), m.to_string()]);
    }
    if let Some(w) = width {
        argv.extend(["--resampleWidth".to_string(), w.to_string()]);
    }
    if let Some(h) = height {
        argv.extend(["--resampleHeight".to_string(), h.to_string()]);
    }
    if let Some(f) = format {
        argv.extend(["-s".to_string(), "format".to_string(), f.to_string()]);
    }
    if let Some(q) = quality {
        argv.extend(["-s".to_string(), "formatOptions".to_string(), q.to_string()]);
    }
    if let Some(out) = out_path {
        argv.extend(["--out".to_string(), out.display().to_string()]);
    }
    Ok(argv)
}

// Reads a pipe up to the cap so a chatty child can never balloon memory.
fn read_capped(mut reader: impl Read, cap: usize) -> String {
    let mut buf = Vec::new();
    let _ = (&mut reader).take((cap as u64) + 1).read_to_end(&mut buf);
    buf.truncate(cap);
    String::from_utf8_lossy(&buf).into_owned()
}

// Runs one sips invocation with the TMP vars pointed at Harness/tmp and a
// wall-clock timeout; returns (exit code, stdout, stderr).
fn run_sips(
    binary: &Path,
    argv: &[String],
    tmp: &Path,
    timeout: Duration,
) -> std::result::Result<(Option<i32>, String, String), String> {
    let mut child = match Command::new(binary)
        .args(argv)
        .env("TMPDIR", tmp)
        .env("TEMP", tmp)
        .env("TMP", tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{} not found; sips ships with every macOS install", binary.display()))
        }
        Err(e) => return Err(format!("cannot run {}: {e}", binary.display())),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, 8_192)));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, 8_192)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
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
                "sips timed out after {} s and was killed",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_thread.join().unwrap_or_default().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default().unwrap_or_default();
    Ok((status.code(), stdout, stderr))
}

// Best-effort pixel size of the finished file; absent from the result when
// the query fails (the transform itself already succeeded).
fn query_size(binary: &Path, tmp: &Path, path: &Path) -> Option<(u32, u32)> {
    let (code, stdout, _) =
        run_sips(binary, &["-g".into(), "pixelWidth".into(), "-g".into(), "pixelHeight".into(), path.display().to_string()], tmp, QUERY_TIMEOUT)
            .ok()?;
    if code != Some(0) {
        return None;
    }
    let dim = |key: &str| -> Option<u32> {
        stdout.lines().find_map(|line| {
            let value = line.trim().strip_prefix(key)?.trim().strip_prefix(':')?.trim();
            value.parse::<u32>().ok()
        })
    };
    Some((dim("pixelWidth")?, dim("pixelHeight")?))
}

pub struct ImageTransformTool;

impl Tool for ImageTransformTool {
    fn name(&self) -> &str {
        "image_transform"
    }
    fn description(&self) -> &str {
        "Image transform via macOS sips: width/height, maxSize, crop {width,height}, rotate deg, format jpeg|png|tiff|heic, quality 1..100 (jpeg only). output=new file; omitted=in place (confirmed). Returns {path, output, bytes, width, height}."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "output": { "type": "string" },
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "maxSize": { "type": "integer" },
                "crop": {
                    "type": "object",
                    "properties": {
                        "width": { "type": "integer" },
                        "height": { "type": "integer" }
                    },
                    "required": ["width", "height"],
                    "additionalProperties": false
                },
                "rotate": { "type": "integer" },
                "format": { "type": "string", "enum": ["jpeg", "png", "tiff", "heic"] },
                "quality": { "type": "integer", "minimum": 1, "maximum": 100 }
            },
            "required": ["path"],
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
        let out_raw = args
            .get("output")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let in_raw = args.get("path").and_then(Value::as_str)?;
        match ctx.project_id {
            Some(pid) => {
                let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid)).ok()?;
                match out_raw {
                    // A new file inside the project folder: the same territory
                    // fs_write skips; an unresolvable output confirms instead.
                    Some(o) => fs_ops::join_rel(&root, o)
                        .map(|_| None)
                        .unwrap_or_else(|e| {
                            Some(format!("image_transform: {}", e.to_string().replace("invalid input: ", "")))
                        }),
                    None => Some(format!(
                        "image_transform перезаписывает {in_raw} на месте (без output)"
                    )),
                }
            }
            None => {
                if let Some(o) = out_raw {
                    let out = resolve_path(o, wd);
                    (!in_default_territory(&out, wd))
                        .then(|| "запись результата image_transform вне Harness/ и Projects/".to_string())
                } else {
                    let path = resolve_path(in_raw, wd);
                    Some(format!(
                        "image_transform перезаписывает {} на месте (без output)",
                        canonicalish(&path).display()
                    ))
                }
            }
        }
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let (label, path, output) = match resolve_io(args, ctx, &wd) {
            Ok(v) => v,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_symlink() => {
                return Ok(ToolResult::err(
                    ToolErrorCode::PermissionDenied,
                    format!("{label} is a symlink; image_transform does not follow symlinks"),
                ))
            }
            Ok(m) if m.is_dir() => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    format!("{} is a directory", path.display()),
                ))
            }
            Ok(_) => {}
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
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !INPUT_EXTS.contains(&ext.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "\"{}\" has unsupported extension \"{ext}\"; supported: {}",
                    path.display(),
                    INPUT_EXTS.join(", ")
                ),
            ));
        }
        let argv = match sips_plan(args, &path, output.as_deref()) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e)),
        };

        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp)?;
        if let Some(out) = &output {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
        }
        let binary = PathBuf::from(SIPS_BINARY);
        let (code, stdout, stderr) = match run_sips(&binary, &argv, &tmp, SIPS_TIMEOUT) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::err(
                    if e.contains("not found") {
                        ToolErrorCode::NotFound
                    } else {
                        ToolErrorCode::ExecutionError
                    },
                    e,
                ))
            }
        };
        if code != Some(0) {
            let detail = if stderr.trim().is_empty() {
                stdout.trim().to_owned()
            } else {
                stderr.trim().to_owned()
            };
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "sips failed (exit {}): {detail}",
                    code.unwrap_or(-1)
                ),
            ));
        }
        let out_path = output.unwrap_or(path);
        let bytes = match fs::metadata(&out_path) {
            Ok(m) if m.is_file() && m.len() > 0 => m.len(),
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!(
                        "sips exited 0 but left no image at {}",
                        out_path.display()
                    ),
                ))
            }
        };
        let mut data = json!({
            "path": label,
            "output": out_path.display().to_string(),
            "bytes": bytes,
        });
        if let Some((w, h)) = query_size(&binary, &tmp, &out_path) {
            data["width"] = json!(w);
            data["height"] = json!(h);
        }
        Ok(ToolResult::ok(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::state::AppState;
    use base64::Engine as _;
    use std::sync::Arc;

    // 4x4 red PNG (IHDR/IDAT/IEND), decoded from base64 so the live tests
    // feed sips a real image without any binary blobs in the source.
    const PNG_4X4: &str = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEElEQVR4nGP4z8AARwzEcQCukw/x0F8jngAAAABJRU5ErkJggg==";

    fn write_png(path: &Path) {
        fs::write(path, base64::engine::general_purpose::STANDARD.decode(PNG_4X4).unwrap()).unwrap();
    }

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

    // Live tests need the real sips; on a machine without it they report and
    // pass instead of failing.
    fn sips_available() -> bool {
        Path::new(SIPS_BINARY).is_file()
    }

    fn dims(path: &Path) -> (u32, u32) {
        let out = Command::new(SIPS_BINARY)
            .args(["-g", "pixelWidth", "-g", "pixelHeight"])
            .arg(path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let dim = |key: &str| {
            text.lines()
                .find_map(|l| l.trim().strip_prefix(key).and_then(|r| r.trim().trim_start_matches(':').trim().parse::<u32>().ok()))
                .unwrap()
        };
        (dim("pixelWidth"), dim("pixelHeight"))
    }

    #[test]
    fn class_policy_and_schema_shape() {
        let t = ImageTransformTool;
        assert_eq!(t.class(), ToolClass::Mutating);
        assert_eq!(t.policy(), PermissionPolicy::CheckConditional);
        let s = t.parameters();
        assert_eq!(s["additionalProperties"], false);
        assert!(validate_args(&s, &json!({ "path": "a.png", "maxSize": 10 })).is_ok());
        assert!(validate_args(&s, &json!({ "path": "a.png", "crop": { "width": 4, "height": 2 } })).is_ok());
        assert!(validate_args(&s, &json!({ "path": "a.png", "rotate": -90 })).is_ok());
        assert!(validate_args(&s, &json!({ "path": "a.png", "format": "jpeg", "quality": 80, "output": "b.jpg" })).is_ok());
        assert!(validate_args(&s, &json!({})).unwrap_err().contains("path"));
        assert!(validate_args(&s, &json!({ "path": "a.png", "crop": { "width": 4 } }))
            .unwrap_err()
            .contains("height"));
        assert!(validate_args(&s, &json!({ "path": "a.png", "format": "gif" }))
            .unwrap_err()
            .contains("enum"));
        assert!(validate_args(&s, &json!({ "path": "a.png", "maxSize": "x" }))
            .unwrap_err()
            .contains("integer"));
        assert!(validate_args(&s, &json!({ "path": "a.png", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
    }

    #[test]
    fn sips_plan_builds_exact_flags_and_rejects_bad_ops() {
        let p = Path::new("/i.png");
        let o = PathBuf::from("/o.jpg");
        assert_eq!(
            sips_plan(&json!({ "maxSize": 64 }), p, Some(&o)).unwrap(),
            ["/i.png", "-Z", "64", "--out", "/o.jpg"]
        );
        assert_eq!(
            sips_plan(&json!({ "width": 800, "height": 600 }), p, None).unwrap(),
            ["/i.png", "--resampleWidth", "800", "--resampleHeight", "600"]
        );
        assert_eq!(
            sips_plan(&json!({ "crop": { "width": 10, "height": 5 } }), p, None).unwrap(),
            ["/i.png", "-c", "5", "10"]
        );
        assert_eq!(sips_plan(&json!({ "rotate": 90 }), p, None).unwrap(), ["/i.png", "-r", "90"]);
        assert_eq!(
            sips_plan(&json!({ "format": "jpeg", "quality": 80 }), p, Some(&o)).unwrap(),
            ["/i.png", "-s", "format", "jpeg", "-s", "formatOptions", "80", "--out", "/o.jpg"]
        );
        // no output flag means in place
        assert!(!sips_plan(&json!({ "rotate": 90 }), p, None).unwrap().iter().any(|a| a == "--out"));

        let err = sips_plan(&json!({}), p, None).unwrap_err();
        assert!(err.contains("at least one operation"), "{err}");
        assert!(sips_plan(&json!({ "maxSize": 0 }), p, None).unwrap_err().contains("maxSize"));
        assert!(sips_plan(&json!({ "crop": { "width": 0, "height": 5 } }), p, None)
            .unwrap_err()
            .contains("crop.width"));
        assert!(sips_plan(&json!({ "quality": 50 }), p, None).unwrap_err().contains("jpeg"));
        assert!(sips_plan(&json!({ "format": "png", "quality": 50 }), p, None)
            .unwrap_err()
            .contains("jpeg"));
        assert!(sips_plan(&json!({ "quality": 0, "format": "jpeg" }), p, None).unwrap_err().contains("quality"));
        assert!(sips_plan(&json!({ "quality": 101, "format": "jpeg" }), p, None).unwrap_err().contains("quality"));
        assert!(sips_plan(&json!({ "format": "gif" }), p, None).unwrap_err().contains("format"));
    }

    #[test]
    fn harness_policy_in_place_overwrites_new_file_and_territory() {
        let (wd, ctx) = harness_env("img-policy");
        let t = ImageTransformTool;
        let inside = t.danger_reason(&json!({ "path": "Harness/a.png", "maxSize": 10 }), &ctx);
        assert!(inside.as_deref().unwrap().contains("на месте"), "{inside:?}");
        let new_inside = t.danger_reason(
            &json!({ "path": "Harness/a.png", "output": "Harness/b.png", "maxSize": 10 }),
            &ctx,
        );
        assert_eq!(new_inside, None, "new file inside Harness/ must not confirm");
        let new_projects = t.danger_reason(
            &json!({ "path": "Harness/a.png", "output": "Projects/p/b.png", "maxSize": 10 }),
            &ctx,
        );
        assert_eq!(new_projects, None, "new file inside Projects/ must not confirm");
        let new_outside = t.danger_reason(
            &json!({ "path": "Harness/a.png", "output": "outside.png", "maxSize": 10 }),
            &ctx,
        );
        assert!(new_outside.as_deref().unwrap().contains("Harness/"), "{new_outside:?}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn project_policy_in_place_confirms_new_file_skips_escape_confirms() {
        let (root, ctx) = project_env("img-policy-proj");
        fs::create_dir_all(root.join("img")).unwrap();
        write_png(&root.join("img/a.png"));
        let t = ImageTransformTool;
        let inside = t.danger_reason(&json!({ "path": "img/a.png", "maxSize": 10 }), &ctx);
        assert!(inside.as_deref().unwrap().contains("на месте"), "{inside:?}");
        let new_file = t.danger_reason(
            &json!({ "path": "img/a.png", "output": "img/b.png", "maxSize": 10 }),
            &ctx,
        );
        assert_eq!(new_file, None, "new file inside the project must not confirm");
        let escape = t.danger_reason(
            &json!({ "path": "img/a.png", "output": "../out.png", "maxSize": 10 }),
            &ctx,
        );
        assert!(escape.is_some(), "escaping output must confirm");
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }

    #[test]
    fn live_resize_convert_rotate_crop_and_in_place() {
        if !sips_available() {
            eprintln!("no /usr/bin/sips on this machine, skipping live image_transform tests");
            return;
        }
        let (wd, ctx) = harness_env("img-live");
        let dir = wd.join("Harness").join("img");
        fs::create_dir_all(&dir).unwrap();
        write_png(&dir.join("a.png"));

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "output": "Harness/img/small.png", "maxSize": 2 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["path"], "Harness/img/a.png");
        assert_eq!(data["output"], dir.join("small.png").display().to_string());
        assert!(data["bytes"].as_u64().unwrap() > 0);
        assert_eq!((data["width"].as_u64().unwrap(), data["height"].as_u64().unwrap()), (2, 2));
        assert_eq!(dims(&dir.join("small.png")), (2, 2));

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "output": "Harness/img/c.jpg", "format": "jpeg", "quality": 80 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!((data["width"].as_u64().unwrap(), data["height"].as_u64().unwrap()), (4, 4));
        assert_eq!(&fs::read(dir.join("c.jpg")).unwrap()[..2], &[0xFF, 0xD8]);

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "output": "Harness/img/rot.png", "rotate": 90 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(dims(&dir.join("rot.png")), (4, 4));

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "output": "Harness/img/crop.png", "crop": { "width": 3, "height": 2 } }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(dims(&dir.join("crop.png")), (3, 2));

        // rotate + crop chained in one sips call
        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "output": "Harness/img/rc.png", "rotate": 90, "crop": { "width": 2, "height": 3 } }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(dims(&dir.join("rc.png")), (2, 3));

        // in place: the original file itself changes
        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "maxSize": 2 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["output"], dir.join("a.png").display().to_string());
        assert_eq!(dims(&dir.join("a.png")), (2, 2));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn missing_input_unsupported_extension_and_invalid_quality_are_errors() {
        let (wd, ctx) = harness_env("img-errors");
        let dir = wd.join("Harness").join("img");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "plain text").unwrap();
        write_png(&dir.join("a.png"));

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/missing.png", "maxSize": 10 }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/notes.txt", "maxSize": 10 }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("unsupported extension"), "{}", err.message);

        // rejected by the plan builder before sips ever runs
        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png", "format": "png", "quality": 50 }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("jpeg"), "{}", err.message);

        let r = ImageTransformTool
            .execute(&json!({ "path": "Harness/img/a.png" }), &ctx)
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("at least one operation"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn live_project_scope_paths_stay_inside_and_write_new_output() {
        if !sips_available() {
            eprintln!("no /usr/bin/sips on this machine, skipping live image_transform tests");
            return;
        }
        let (root, ctx) = project_env("img-live-proj");
        // project_root canonicalizes (/var -> /private/var on macOS).
        let root = root.canonicalize().unwrap();
        fs::create_dir_all(root.join("img")).unwrap();
        write_png(&root.join("img/a.png"));

        let r = ImageTransformTool
            .execute(&json!({ "path": "img/a.png", "output": "img/small.png", "maxSize": 2 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["path"], "img/a.png");
        assert_eq!(data["output"], root.join("img/small.png").display().to_string());
        assert_eq!((data["width"].as_u64().unwrap(), data["height"].as_u64().unwrap()), (2, 2));

        for bad in ["../x.png", "/etc/passwd", "a/../../x.png"] {
            let r = ImageTransformTool
                .execute(&json!({ "path": bad, "maxSize": 10 }), &ctx)
                .unwrap();
            assert_eq!(r.error.unwrap().code, "permission_denied", "{bad:?}");
        }
        fs::remove_dir_all(ctx.working_dir.unwrap()).unwrap();
    }
}
