use super::result::{read_capped, ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OSASCRIPT_BINARY: &str = "/usr/bin/osascript";
const UI_TIMEOUT: Duration = Duration::from_secs(10);
const STDERR_CAP: usize = 8_192;
const STDOUT_CAP: usize = 200_000;
const MAX_COORD: i64 = 100_000;
const FIELD_SEP: char = '\u{1f}';

pub(crate) const ACCESSIBILITY_HINT: &str = "Нет доступа к управлению интерфейсом: включите разрешение для этого приложения в «Системные настройки» → «Конфиденциальность и безопасность» → «Универсальный доступ» (Accessibility), затем повторите.";
pub(crate) const AUTOMATION_HINT: &str = "Нет разрешения на автоматизацию: включите разрешение для этого приложения в «Системные настройки» → «Конфиденциальность и безопасность» → «Автоматизация» (Automation), затем повторите.";

const AX_PROBE: &str = "tell application \"System Events\"\n\tif not (UI elements enabled) then error \"AX_DISABLED\"\nend tell";

pub(crate) fn as_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn key_code(key: &str) -> Option<u32> {
    Some(match key.to_ascii_lowercase().as_str() {
        "return" | "enter" => 36,
        "tab" => 48,
        "escape" | "esc" => 53,
        "delete" | "backspace" => 51,
        "space" => 49,
        "up" => 126,
        "down" => 125,
        "left" => 123,
        "right" => 124,
        "home" => 115,
        "end" => 119,
        "pageup" | "page up" => 116,
        "pagedown" | "page down" => 121,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    })
}

fn modifier_clause(args: &Value) -> std::result::Result<String, String> {
    match args.get("modifiers") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::Array(list)) => {
            let mut parts: Vec<&str> = Vec::new();
            for m in list {
                let name = m.as_str().ok_or("modifiers must be an array of strings")?;
                parts.push(match name.to_ascii_lowercase().as_str() {
                    "cmd" | "command" => "command down",
                    "ctrl" | "control" => "control down",
                    "opt" | "option" | "alt" => "option down",
                    "shift" => "shift down",
                    other => {
                        return Err(format!(
                            "unknown modifier \"{other}\": use cmd, ctrl, opt or shift"
                        ))
                    }
                });
            }
            if parts.is_empty() {
                Ok(String::new())
            } else {
                Ok(format!(" using {{{}}}", parts.join(", ")))
            }
        }
        Some(_) => Err("modifiers must be an array of strings".to_string()),
    }
}

fn key_stmt(key: &str, mods: &str) -> std::result::Result<String, String> {
    if let Some(code) = key_code(key) {
        return Ok(format!("key code {code}{mods}"));
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() == 1 && !chars[0].is_control() {
        return Ok(format!("keystroke {}{mods}", as_quote(key)));
    }
    Err(format!(
        "unknown key \"{key}\": use a named key (return, enter, tab, escape, delete, backspace, space, up, down, left, right, home, end, pageup, pagedown, f1-f12) or a single character"
    ))
}

fn coord(args: &Value, name: &str) -> std::result::Result<i64, String> {
    let value = args
        .get(name)
        .filter(|v| !v.is_null())
        .ok_or_else(|| format!("{name} is required for click"))?;
    let float = value
        .as_f64()
        .ok_or_else(|| format!("{name} must be a number"))?;
    if !float.is_finite() {
        return Err(format!("{name} must be a finite number"));
    }
    let int = float.round() as i64;
    if !(0..=MAX_COORD).contains(&int) {
        return Err(format!("{name} must be between 0 and {MAX_COORD}"));
    }
    Ok(int)
}

fn non_empty<'a>(args: &'a Value, name: &str, action: &str) -> std::result::Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("{action} needs a non-empty {name}"))
}

fn window_list_body() -> String {
    format!(
        "set out to {{}}\ntell application \"System Events\"\n\trepeat with p in (every application process)\n\t\ttry\n\t\t\trepeat with w in (windows of p)\n\t\t\t\tset pName to name of p\n\t\t\t\ttry\n\t\t\t\t\tset wName to name of w\n\t\t\t\ton error\n\t\t\t\t\tset wName to \"\"\n\t\t\t\tend try\n\t\t\t\ttry\n\t\t\t\t\tset pos to position of w\n\t\t\t\t\tset sz to size of w\n\t\t\t\t\tset b to ((item 1 of pos) as text) & \",\" & ((item 2 of pos) as text) & \",\" & ((item 1 of sz) as text) & \",\" & ((item 2 of sz) as text)\n\t\t\t\ton error\n\t\t\t\t\tset b to \"\"\n\t\t\t\tend try\n\t\t\t\tset end of out to pName & (character id 31) & wName & (character id 31) & b\n\t\t\tend repeat\n\t\tend try\n\tend repeat\nend tell\nset text item delimiters to linefeed\nset s to out as text\nset text item delimiters to \"\"\nreturn s"
    )
}

fn build_script(action: &str, args: &Value) -> std::result::Result<(String, Value), String> {
    match action {
        "window_list" => Ok((format!("{AX_PROBE}\n{}", window_list_body()), json!({}))),
        "window_focus" => {
            let app = args.get("app").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty());
            if app.is_none() && title.is_none() {
                return Err("window_focus needs app and/or title".to_string());
            }
            let body = match (app, title) {
                (Some(app), Some(title)) => format!(
                    "tell application \"System Events\"\n\ttell application process {} to set frontmost to true\n\ttell application process {}\n\t\ttry\n\t\t\tperform action \"AXRaise\" of window {}\n\t\ton error\n\t\t\treturn \"NO_WINDOW\"\n\t\tend try\n\tend tell\nend tell\nreturn \"OK\"",
                    as_quote(app),
                    as_quote(app),
                    as_quote(title)
                ),
                (Some(app), None) => format!(
                    "tell application \"System Events\"\n\tset frontmost of application process {} to true\nend tell\nreturn \"OK\"",
                    as_quote(app)
                ),
                (None, Some(title)) => format!(
                    "set found to false\ntell application \"System Events\"\n\trepeat with p in (every application process)\n\t\ttry\n\t\t\trepeat with w in (windows of p)\n\t\t\t\tif (name of w) is {} then\n\t\t\t\t\tset frontmost of p to true\n\t\t\t\t\tperform action \"AXRaise\" of w\n\t\t\t\t\tset found to true\n\t\t\t\tend if\n\t\t\tend repeat\n\t\tend try\n\t\tif found then exit repeat\n\tend repeat\nend tell\nreturn found",
                    as_quote(title)
                ),
                _ => unreachable!(),
            };
            Ok((
                format!("{AX_PROBE}\n{body}"),
                json!({ "action": action, "app": app, "title": title }),
            ))
        }
        "activate" => {
            let app = non_empty(args, "app", "activate")?;
            Ok((
                format!(
                    "{AX_PROBE}\ntell application \"System Events\"\n\tset frontmost of application process {} to true\nend tell",
                    as_quote(app)
                ),
                json!({ "action": action, "app": app }),
            ))
        }
        "click" => {
            let x = coord(args, "x")?;
            let y = coord(args, "y")?;
            let button = args
                .get("button")
                .and_then(Value::as_str)
                .map(|b| b.to_ascii_lowercase())
                .unwrap_or_else(|| "left".to_string());
            if !["left", "right"].contains(&button.as_str()) {
                return Err("button must be left or right".to_string());
            }
            let body = if button == "right" {
                format!(
                    "tell application \"System Events\"\n\tkey down control\n\tclick at {{{x}, {y}}}\n\tkey up control\nend tell"
                )
            } else {
                format!("tell application \"System Events\"\n\tclick at {{{x}, {y}}}\nend tell")
            };
            Ok((
                format!("{AX_PROBE}\n{body}"),
                json!({ "action": action, "x": x, "y": y, "button": button }),
            ))
        }
        "type" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or("type needs non-empty text")?;
            let mut body = String::from("tell application \"System Events\"\n");
            for (i, segment) in text.split('\n').enumerate() {
                if i > 0 {
                    body.push_str("\tkey code 36\n");
                }
                if !segment.is_empty() {
                    body.push_str(&format!("\tkeystroke {}\n", as_quote(segment)));
                }
            }
            body.push_str("end tell");
            Ok((
                format!("{AX_PROBE}\n{body}"),
                json!({ "action": action, "chars": text.chars().count() }),
            ))
        }
        "key" | "hotkey" => {
            let key = non_empty(args, "key", action)?;
            let mods = modifier_clause(args)?;
            let stmt = key_stmt(key, &mods)?;
            Ok((
                format!("{AX_PROBE}\ntell application \"System Events\"\n\t{stmt}\nend tell"),
                json!({ "action": action, "key": key, "modifiers": args.get("modifiers").cloned().unwrap_or(json!([])) }),
            ))
        }
        "menu" => {
            let app = non_empty(args, "app", "menu")?;
            let menu = non_empty(args, "menu", "menu")?;
            let item = non_empty(args, "item", "menu")?;
            Ok((
                format!(
                    "{AX_PROBE}\ntell application \"System Events\"\n\ttell application process {}\n\t\tclick menu item {} of menu {} of menu bar item {} of menu bar 1\n\tend tell\nend tell",
                    as_quote(app),
                    as_quote(item),
                    as_quote(menu),
                    as_quote(menu)
                ),
                json!({ "action": action, "app": app, "menu": menu, "item": item }),
            ))
        }
        other => Err(format!("unknown action \"{other}\"")),
    }
}

fn parse_windows(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split(FIELD_SEP);
            let app = parts.next().unwrap_or("").to_string();
            let title = parts.next().unwrap_or("").to_string();
            let bounds: Vec<i64> = parts
                .next()
                .unwrap_or("")
                .split(',')
                .filter_map(|v| v.trim().parse::<i64>().ok())
                .collect();
            json!({
                "app": app,
                "title": title,
                "x": bounds.get(0),
                "y": bounds.get(1),
                "w": bounds.get(2),
                "h": bounds.get(3),
            })
        })
        .collect()
}

pub(crate) fn is_accessibility_error(text: &str) -> bool {
    let low = text.to_lowercase();
    low.contains("ax_disabled")
        || low.contains("assistive access")
        || low.contains("упрощенного доступа")
        || low.contains("упрощённого доступа")
        || low.contains("вспомогательного доступа")
        || text.contains("-25211")
        || text.contains("-1719")
}

pub(crate) fn is_automation_error(text: &str) -> bool {
    let low = text.to_lowercase();
    low.contains("not authorized to send apple events")
        || low.contains("не разрешена отправка событий")
        || low.contains("не разрешено отправлять события")
        || text.contains("-1743")
}

pub(crate) fn map_osascript_failure(code: Option<i32>, stderr: &str) -> ToolResult {
    let detail = stderr.trim();
    if is_accessibility_error(stderr) {
        return ToolResult::err(
            ToolErrorCode::PermissionDenied,
            format!(
                "macOS отказал в доступе к управлению интерфейсом (osascript, exit {}): {detail}. {ACCESSIBILITY_HINT}",
                code.unwrap_or(-1)
            ),
        );
    }
    if is_automation_error(stderr) {
        return ToolResult::err(
            ToolErrorCode::PermissionDenied,
            format!(
                "macOS отказал в автоматизации (osascript, exit {}): {detail}. {AUTOMATION_HINT}",
                code.unwrap_or(-1)
            ),
        );
    }
    let low = stderr.to_lowercase();
    if low.contains("application process") && (low.contains("can't get") || low.contains("не удается получить") || low.contains("не удаётся получить")) {
        return ToolResult::err(
            ToolErrorCode::NotFound,
            "the application is not running (no such application process)".to_string(),
        );
    }
    ToolResult::err(
        ToolErrorCode::ExecutionError,
        format!(
            "osascript failed (exit {}): {detail}",
            code.unwrap_or(-1)
        ),
    )
}

pub(crate) fn run_osascript(
    binary: &Path,
    script: &str,
    timeout: Duration,
) -> std::result::Result<(Option<i32>, String, String), String> {
    let mut child = match Command::new(binary)
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "{} not found; osascript ships with every macOS install",
                binary.display()
            ))
        }
        Err(e) => return Err(format!("cannot run {}: {e}", binary.display())),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, STDOUT_CAP).0));
    let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, STDERR_CAP).0));
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
                "osascript timed out after {} s and was killed",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_thread.join().unwrap_or_default().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default().unwrap_or_default();
    Ok((status.code(), stdout, stderr))
}

fn danger_for(action: &str) -> Option<&'static str> {
    match action {
        "window_list" => None,
        "window_focus" => Some("переключить приложение или окно на передний план"),
        "activate" => Some("активировать приложение"),
        "click" => Some("клик мышью по координатам экрана"),
        "type" => Some("ввести текст в активное приложение"),
        "key" => Some("нажать клавишу"),
        "hotkey" => Some("нажать комбинацию клавиш"),
        "menu" => Some("выбрать пункт меню приложения"),
        _ => Some("управление интерфейсом macOS"),
    }
}

pub struct MacosUiTool {
    binary: PathBuf,
    timeout: Duration,
}

impl Default for MacosUiTool {
    fn default() -> Self {
        MacosUiTool {
            binary: PathBuf::from(OSASCRIPT_BINARY),
            timeout: UI_TIMEOUT,
        }
    }
}

impl MacosUiTool {
    #[cfg(test)]
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        MacosUiTool {
            binary: binary.into(),
            timeout: UI_TIMEOUT,
        }
    }
}

impl Tool for MacosUiTool {
    fn name(&self) -> &str {
        "macos_ui"
    }
    fn description(&self) -> &str {
        "macOS UI automation via osascript/System Events (needs Accessibility permission; every mutating action is confirmed).\nwindow_list: list every window as {app,title,x,y,w,h} (read-only). window_focus: bring app and/or window (exact title) to front. activate: bring a running app to front (app). click: click at screen coordinates x,y from the top-left origin; button left|right (right is Control+click). type: type text into the focused app; each newline presses Return. key: press a named key (return, enter, tab, escape, delete, backspace, space, up, down, left, right, home, end, pageup, pagedown, f1-f12) or a single character, optional modifiers [cmd,ctrl,opt,shift]. hotkey: key + modifiers combo. menu: choose a menu bar item (app, menu, item). Permission failures name the System Settings pane to fix."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["window_list", "window_focus", "activate", "click", "type", "key", "hotkey", "menu"] },
                "app": { "type": "string" },
                "title": { "type": "string" },
                "x": { "type": "number" },
                "y": { "type": "number" },
                "button": { "type": "string", "enum": ["left", "right"] },
                "text": { "type": "string" },
                "key": { "type": "string" },
                "modifiers": { "type": "array", "items": { "type": "string", "enum": ["cmd", "ctrl", "opt", "shift"] } },
                "menu": { "type": "string" },
                "item": { "type": "string" }
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
        danger_for(args.get("action").and_then(Value::as_str).unwrap_or("")).map(str::to_string)
    }
    fn max_result_len(&self) -> usize {
        60_000
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or_default();
        let (script, echo) = match build_script(action, args) {
            Ok(built) => built,
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
        let (code, stdout, stderr) = match run_osascript(&self.binary, &script, self.timeout) {
            Ok(runs) => runs,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        if code != Some(0) {
            return Ok(map_osascript_failure(code, &stderr));
        }
        match action {
            "window_list" => {
                let windows = parse_windows(&stdout);
                Ok(ToolResult::ok(json!({
                    "action": action,
                    "count": windows.len(),
                    "windows": windows,
                }))
                .capped(self.max_result_len()))
            }
            "window_focus" => {
                let answer = stdout.trim();
                if answer == "NO_WINDOW" {
                    return Ok(ToolResult::err(
                        ToolErrorCode::NotFound,
                        format!(
                            "window \"{}\" not found in app \"{}\"",
                            args["title"].as_str().unwrap_or(""),
                            args["app"].as_str().unwrap_or("")
                        ),
                    ));
                }
                if answer == "false" {
                    return Ok(ToolResult::err(
                        ToolErrorCode::NotFound,
                        format!(
                            "no window with title \"{}\" in any running app",
                            args["title"].as_str().unwrap_or("")
                        ),
                    ));
                }
                Ok(ToolResult::ok(echo))
            }
            _ => Ok(ToolResult::ok(echo)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fake_osascript(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("fake-osascript");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn class_policy_and_per_action_confirmation() {
        let tool = MacosUiTool::default();
        assert_eq!(tool.class(), ToolClass::Mutating);
        assert_eq!(tool.policy(), PermissionPolicy::CheckConditional);
        assert!(tool
            .danger_reason(&json!({ "action": "window_list" }), &ToolCtx::default())
            .is_none());
        for action in ["window_focus", "activate", "click", "type", "key", "hotkey", "menu"] {
            let reason = tool.danger_reason(&json!({ "action": action }), &ToolCtx::default());
            assert!(reason.is_some(), "{action} must confirm");
            assert!(!reason.unwrap().is_empty());
        }
        assert!(tool
            .danger_reason(&json!({ "action": "weird" }), &ToolCtx::default())
            .is_some());
        assert!(tool.danger_reason(&json!({}), &ToolCtx::default()).is_some());
    }

    #[test]
    fn schema_shape_and_validation() {
        let schema = MacosUiTool::default().parameters();
        assert_eq!(schema["additionalProperties"], false);
        assert!(validate_args(&schema, &json!({ "action": "window_list" })).is_ok());
        assert!(validate_args(
            &schema,
            &json!({ "action": "click", "x": 10, "y": 20.5, "button": "right" })
        )
        .is_ok());
        assert!(validate_args(
            &schema,
            &json!({ "action": "hotkey", "key": "a", "modifiers": ["cmd", "shift"] })
        )
        .is_ok());
        assert!(validate_args(&schema, &json!({})).unwrap_err().contains("action"));
        assert!(validate_args(&schema, &json!({ "action": "explode" }))
            .unwrap_err()
            .contains("enum"));
        assert!(validate_args(&schema, &json!({ "action": "click", "x": "ten" }))
            .unwrap_err()
            .contains("number"));
        assert!(validate_args(
            &schema,
            &json!({ "action": "key", "key": "a", "modifiers": ["hyper"] })
        )
        .unwrap_err()
        .contains("enum"));
        assert!(validate_args(&schema, &json!({ "action": "click", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
    }

    #[test]
    fn as_quote_escapes_quotes_backslashes_and_control_chars() {
        assert_eq!(as_quote("plain"), "\"plain\"");
        assert_eq!(as_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(as_quote("a\nb\rc\td"), "\"a\\nb\\rc\\td\"");
        assert_eq!(as_quote("a\u{7}b"), "\"ab\"");
        assert_eq!(as_quote("Системные \"Настройки\""), "\"Системные \\\"Настройки\\\"\"");
    }

    #[test]
    fn injection_never_reaches_the_script_unescaped() {
        let (script, _) = build_script(
            "activate",
            &json!({ "action": "activate", "app": "X\" & do shell script \"rm -rf / & \"" }),
        )
        .unwrap();
        assert!(!script.contains("\" & do shell script \""));
        assert!(script.contains("\\\" & do shell script \\\""));
        let (script, _) = build_script(
            "menu",
            &json!({ "action": "menu", "app": "A", "menu": "M\\", "item": "I\nJ" }),
        )
        .unwrap();
        assert!(script.contains("\"M\\\\\""));
        assert!(script.contains("\"I\\nJ\""));
    }

    #[test]
    fn click_scripts_use_validated_integer_coordinates() {
        let (script, echo) = build_script("click", &json!({ "action": "click", "x": 10, "y": 20 })).unwrap();
        assert!(script.contains("click at {10, 20}"));
        assert!(!script.contains("key down control"));
        assert_eq!(echo["x"], 10);
        assert_eq!(echo["button"], "left");
        let (script, echo) = build_script(
            "click",
            &json!({ "action": "click", "x": 10.4, "y": 20.6, "button": "right" }),
        )
        .unwrap();
        assert!(script.contains("key down control"));
        assert!(script.contains("click at {10, 21}"));
        assert_eq!(echo["button"], "right");
        assert!(build_script("click", &json!({ "action": "click" })).is_err());
        assert!(build_script("click", &json!({ "action": "click", "x": -1, "y": 5 })).is_err());
        assert!(build_script("click", &json!({ "action": "click", "x": 1e9, "y": 5 })).is_err());
        assert!(build_script(
            "click",
            &json!({ "action": "click", "x": 5, "y": 5, "button": "middle" })
        )
        .is_err());
    }

    #[test]
    fn type_splits_newlines_into_return_key_presses() {
        let (script, echo) = build_script("type", &json!({ "action": "type", "text": "pri\nsec" })).unwrap();
        assert!(script.contains("keystroke \"pri\""));
        assert!(script.contains("key code 36"));
        assert!(script.contains("keystroke \"sec\""));
        assert_eq!(echo["chars"], 7);
        assert!(build_script("type", &json!({ "action": "type", "text": "" })).is_err());
        assert!(build_script("type", &json!({ "action": "type" })).is_err());
    }

    #[test]
    fn key_and_hotkey_build_key_code_or_keystroke_statements() {
        let (script, _) = build_script("key", &json!({ "action": "key", "key": "escape" })).unwrap();
        assert!(script.contains("key code 53"));
        let (script, _) = build_script(
            "hotkey",
            &json!({ "action": "hotkey", "key": "a", "modifiers": ["cmd", "shift"] }),
        )
        .unwrap();
        assert!(script.contains("keystroke \"a\" using {command down, shift down}"));
        let (script, _) = build_script("hotkey", &json!({ "action": "hotkey", "key": "tab", "modifiers": ["ctrl"] })).unwrap();
        assert!(script.contains("key code 48 using {control down}"));
        assert!(build_script("key", &json!({ "action": "key", "key": "bogus" })).is_err());
        assert!(build_script("key", &json!({ "action": "key", "key": "ab" })).is_err());
        assert!(build_script(
            "hotkey",
            &json!({ "action": "hotkey", "key": "a", "modifiers": "cmd" })
        )
        .is_err());
    }

    #[test]
    fn window_focus_activate_and_menu_build_expected_scripts() {
        let (script, _) = build_script(
            "window_focus",
            &json!({ "action": "window_focus", "app": "Finder", "title": "Desktop" }),
        )
        .unwrap();
        assert!(script.contains("application process \"Finder\""));
        assert!(script.contains("AXRaise\" of window \"Desktop\""));
        let (script, _) = build_script("window_focus", &json!({ "action": "window_focus", "title": "Desktop" })).unwrap();
        assert!(script.contains("repeat with p in (every application process)"));
        assert!(build_script("window_focus", &json!({ "action": "window_focus" })).is_err());
        let (script, _) = build_script("activate", &json!({ "action": "activate", "app": "Finder" })).unwrap();
        assert!(script.contains("set frontmost of application process \"Finder\" to true"));
        assert!(build_script("activate", &json!({ "action": "activate" })).is_err());
        let (script, _) = build_script(
            "menu",
            &json!({ "action": "menu", "app": "Finder", "menu": "File", "item": "New Folder" }),
        )
        .unwrap();
        assert!(script.contains("click menu item \"New Folder\" of menu \"File\" of menu bar item \"File\" of menu bar 1"));
        assert!(build_script("menu", &json!({ "action": "menu", "app": "A", "menu": "M" })).is_err());
        assert!(build_script("unknown", &json!({ "action": "unknown" })).is_err());
    }

    #[test]
    fn parse_windows_builds_structured_entries() {
        let sep = FIELD_SEP;
        let stdout = format!("Finder{sep}Desktop{sep}0,0,1920,1080\nCode{sep}main.rs{sep}10,20,800,600\nGhost{sep}{sep}\n");
        let windows = parse_windows(&stdout);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0]["app"], "Finder");
        assert_eq!(windows[0]["title"], "Desktop");
        assert_eq!(windows[0]["w"], 1920);
        assert_eq!(windows[1]["x"], 10);
        assert_eq!(windows[2]["title"], "");
        assert!(windows[2]["x"].is_null());
    }

    #[test]
    fn permission_errors_map_to_permission_denied_with_russian_hints() {
        let denied = map_osascript_failure(
            Some(1),
            "59:61: execution error: Получена ошибка от «System Events»: Функции Упрощенного доступа для «osascript» не разрешены. (-25211)",
        );
        let err = denied.error.unwrap();
        assert_eq!(err.code, "permission_denied");
        assert!(err.message.contains("Универсальный доступ"), "{}", err.message);
        assert!(err.message.contains("Конфиденциальность"), "{}", err.message);

        let denied = map_osascript_failure(
            Some(1),
            "osascript: error: execution error: osascript is not allowed assistive access. (-1719)",
        );
        assert_eq!(denied.error.as_ref().unwrap().code, "permission_denied");

        let denied = map_osascript_failure(
            Some(1),
            "execution error: Не разрешена отправка событий Apple от «osascript» к «System Events». (-1743)",
        );
        let err = denied.error.unwrap();
        assert_eq!(err.code, "permission_denied");
        assert!(err.message.contains("Автоматизация"), "{}", err.message);

        let denied = map_osascript_failure(Some(1), "AX_DISABLED (-2700)");
        assert_eq!(denied.error.as_ref().unwrap().code, "permission_denied");

        let other = map_osascript_failure(Some(1), "syntax error: Can't make some data into the incorrect type.");
        assert_eq!(other.error.as_ref().unwrap().code, "execution_error");

        let missing_app = map_osascript_failure(
            Some(1),
            "68:72: execution error: Не удается получить application process \"Ghost\". (-1728)",
        );
        assert_eq!(missing_app.error.as_ref().unwrap().code, "not_found");
    }

    #[test]
    fn window_list_returns_structured_windows_from_fake_osascript() {
        let dir = temp_test_dir("ui-window-list");
        let fake = fake_osascript(
            &dir,
            "printf 'Finder\\037Desktop\\0370,0,1920,1080\\nCode\\037main.rs\\03710,20,800,600\\n'",
        );
        let r = MacosUiTool::with_binary(fake)
            .execute(&json!({ "action": "window_list" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["action"], "window_list");
        assert_eq!(data["count"], 2);
        assert_eq!(data["windows"][0]["app"], "Finder");
        assert_eq!(data["windows"][1]["h"], 600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mutating_actions_pass_the_probe_and_the_escaped_script_to_osascript() {
        let dir = temp_test_dir("ui-argv");
        let capture = dir.join("script.txt");
        let fake = fake_osascript(&dir, &format!("printf '%s' \"$2\" > '{}'", capture.display()));
        let r = MacosUiTool::with_binary(fake)
            .execute(
                &json!({ "action": "click", "x": 12, "y": 34, "button": "right" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let script = fs::read_to_string(&capture).unwrap();
        assert!(script.contains("UI elements enabled"));
        assert!(script.contains("click at {12, 34}"));
        assert!(script.contains("key down control"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accessibility_denial_surfaces_as_permission_denied_not_empty_output() {
        let dir = temp_test_dir("ui-permission");
        let fake = fake_osascript(
            &dir,
            "echo 'Получена ошибка от «System Events»: Функции Упрощенного доступа для «osascript» не разрешены. (-25211)' >&2\nexit 1",
        );
        let r = MacosUiTool::with_binary(fake)
            .execute(&json!({ "action": "window_list" }), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "permission_denied");
        assert!(err.message.contains("Универсальный доступ"), "{}", err.message);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn window_focus_reports_not_found_for_missing_windows() {
        let dir = temp_test_dir("ui-focus-miss");
        let fake = fake_osascript(&dir, "echo false");
        let r = MacosUiTool::with_binary(fake.clone())
            .execute(
                &json!({ "action": "window_focus", "title": "Ghost" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "not_found");
        let fake = fake_osascript(&dir, "echo NO_WINDOW");
        let r = MacosUiTool::with_binary(fake)
            .execute(
                &json!({ "action": "window_focus", "app": "Finder", "title": "Ghost" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "not_found");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_osascript_maps_to_not_found() {
        let dir = temp_test_dir("ui-nobinary");
        let r = MacosUiTool::with_binary(dir.join("no-osascript"))
            .execute(&json!({ "action": "window_list" }), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("osascript"), "{}", err.message);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn timeout_kills_a_stuck_osascript() {
        let dir = temp_test_dir("ui-timeout");
        let fake = fake_osascript(&dir, "sleep 30");
        let tool = MacosUiTool {
            binary: fake,
            timeout: Duration::from_millis(300),
        };
        let r = tool
            .execute(&json!({ "action": "activate", "app": "Finder" }), &ToolCtx::default())
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("timed out"), "{}", err.message);
        fs::remove_dir_all(&dir).unwrap();
    }
}
