// Harness browser tool (SPEC 5.2 #3): drives the installed Chrome/Chromium
// through the cdp module. The browser session survives across tool calls in
// one app process (open, then text/click/screenshot/eval/wait/type/press/
// select/extract/upload/download/tab_* on the active tab) and is killed
// after IDLE_SECS of inactivity or when the tool errors. All artifacts
// (profile, screenshots, downloads) stay under Harness/tmp/. Missing Chrome
// is a plain execution_error: the tool is always registered, it just degrades.
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::cdp::{self, Browser};
use crate::error::AppError;
use crate::paths;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const IDLE_SECS: u64 = 120;
const TEXT_CAP: usize = 20_000;
const EXTRACT_CAP: usize = 50_000;
const WAIT_TIMEOUT_MS: i64 = 10_000;
const WAIT_TIMEOUT_MAX_MS: i64 = 60_000;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_POLL: Duration = Duration::from_millis(500);

struct Session<B = Browser> {
    browser: B,
    wd: PathBuf,
    headless: bool,
    last_used: Instant,
}

// Single-user desktop app: one shared browser session for the whole process.
struct BrowserRuntime<B = Browser> {
    slot: Mutex<Option<Session<B>>>,
}

impl<B> BrowserRuntime<B> {
    const fn new() -> Self {
        Self { slot: Mutex::new(None) }
    }

    fn with_session<R>(
        &self,
        wd: &Path,
        headless: bool,
        launch: impl FnOnce() -> std::result::Result<B, String>,
        action: impl FnOnce(&mut Session<B>) -> crate::error::Result<R>,
    ) -> std::result::Result<R, String> {
        let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.as_ref() {
            let stale = session.wd != wd
                || session.headless != headless
                || session.last_used.elapsed() > Duration::from_secs(IDLE_SECS);
            if stale {
                *guard = None; // Drop kills the old Chrome
            }
        }
        if guard.is_none() {
            match launch() {
                Ok(browser) => {
                    *guard = Some(Session {
                        browser,
                        wd: wd.to_path_buf(),
                        headless,
                        last_used: Instant::now(),
                    });
                }
                Err(message) => {
                    *guard = None;
                    return Err(message);
                }
            }
        }
        let session = guard.as_mut().expect("session set before action");
        match action(session) {
            Ok(value) => Ok(value),
            Err(e) => {
                *guard = None; // any CDP failure drops (kills) the browser
                Err(e.to_string())
            }
        }
    }

    fn kill(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[allow(dead_code)] // runtime exit hook; exercised by cfg(test), not wired to app exit yet
    fn shutdown(&self) {
        self.kill();
    }
}

static RUNTIME: BrowserRuntime = BrowserRuntime::new();

fn launch_browser(tmp: &Path, headless: bool) -> std::result::Result<Browser, String> {
    let chrome = match cdp::find_chrome() {
        Ok(chrome) => chrome,
        Err(e) => return Err(e.to_string()),
    };
    if let Err(e) = std::fs::create_dir_all(tmp) {
        return Err(e.to_string());
    }
    Browser::launch(&chrome, tmp, headless).map_err(|e| e.to_string())
}

fn allowed_url(url: &str) -> bool {
    ["http://", "https://", "data:"].iter().any(|p| url.starts_with(p))
}

fn truncate_text(text: &str) -> (String, bool) {
    if text.chars().count() <= TEXT_CAP {
        return (text.to_string(), false);
    }
    let cut: String = text.chars().take(TEXT_CAP).collect();
    (cut, true)
}

fn clamp_timeout_ms(ms: Option<i64>) -> Duration {
    Duration::from_millis(ms.unwrap_or(WAIT_TIMEOUT_MS).clamp(1, WAIT_TIMEOUT_MAX_MS) as u64)
}

fn clamp_max_chars(chars: Option<i64>) -> usize {
    match chars {
        Some(chars) => chars.clamp(1, EXTRACT_CAP as i64) as usize,
        None => EXTRACT_CAP,
    }
}

// Returns the JSON value as-is when it fits the cap, otherwise the cut
// serialized form flagged as truncated (never a half-broken JSON blob).
fn cap_json(value: Value, max_chars: usize) -> Value {
    let serialized = value.to_string();
    if serialized.chars().count() <= max_chars {
        json!({ "value": value, "truncated": false })
    } else {
        let cut: String = serialized.chars().take(max_chars).collect();
        json!({ "value": cut, "truncated": true })
    }
}

// Per-action policy (SPEC 5.2): read-only actions never confirm; anything
// that acts on the page, the network or the local disk confirms. Unknown
// actions confirm too: the safe default is the confirmation queue.
fn danger_for(action: &str) -> Option<&'static str> {
    match action {
        "text" | "screenshot" | "tab_list" | "extract" | "wait" => None,
        "open" => Some("открыть страницу в браузере"),
        "click" => Some("клик по элементу на странице"),
        "eval" => Some("выполнить JavaScript на странице"),
        "type" => Some("ввести текст в поле на странице"),
        "press" => Some("нажать клавишу на странице"),
        "select" => Some("выбрать пункт в выпадающем списке на странице"),
        "upload" => Some("загрузить локальный файл в поле input[type=file] на странице"),
        "download" => Some("скачать файл из браузера в рабочую папку"),
        "tab_open" => Some("открыть новую вкладку в браузере"),
        "tab_close" => Some("закрыть вкладку браузера"),
        "tab_switch" => Some("переключить активную вкладку браузера"),
        _ => Some("управление браузером"),
    }
}

pub struct BrowserTool;

impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }
    fn description(&self) -> &str {
        "Drive Chrome/Chromium via CDP: open (url), text, screenshot, click (selector), eval, wait, type, press, select, extract, upload (selector, path), download (selector?, path), tab_open (url), tab_list, tab_close, tab_switch (tabId)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["open", "text", "screenshot", "click", "eval", "wait", "type", "press", "select", "extract", "upload", "download", "tab_open", "tab_list", "tab_close", "tab_switch"] },
                "url": { "type": "string" },
                "selector": { "type": "string" },
                "expression": { "type": "string" },
                "text": { "type": "string" },
                "key": { "type": "string" },
                "value": { "type": "string" },
                "path": { "type": "string" },
                "tabId": { "type": "string" },
                "timeoutMs": { "type": "integer" },
                "maxChars": { "type": "integer" },
                "headless": { "type": "boolean" }
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
        // 55 k so a 50 k extract result still fits inside the capped envelope.
        55_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> crate::error::Result<ToolResult> {
        let wd = match ctx.working_dir.as_deref() {
            Some(wd) => wd.to_path_buf(),
            None => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, "working directory is not configured")),
        };
        let action = args["action"].as_str().unwrap_or_default();
        let url = args.get("url").and_then(Value::as_str);
        let selector = args.get("selector").and_then(Value::as_str);
        let expression = args.get("expression").and_then(Value::as_str);
        let text = args.get("text").and_then(Value::as_str);
        let key = args.get("key").and_then(Value::as_str);
        let value = args.get("value").and_then(Value::as_str);
        let path_arg = args.get("path").and_then(Value::as_str);
        let tab_id = args.get("tabId").and_then(Value::as_str);
        let headless = args.get("headless").and_then(Value::as_bool).unwrap_or(true);
        let timeout = clamp_timeout_ms(args.get("timeoutMs").and_then(Value::as_i64));
        let max_chars = clamp_max_chars(args.get("maxChars").and_then(Value::as_i64));

        let invalid = |msg: &str| Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
        match action {
            "open" if !url.is_some_and(allowed_url) => {
                return invalid("open needs a url with an http://, https:// or data: scheme")
            }
            "click" if selector.unwrap_or_default().is_empty() => return invalid("click needs a selector"),
            "eval" if expression.unwrap_or_default().is_empty() => return invalid("eval needs an expression"),
            "wait" if selector.unwrap_or_default().is_empty() && text.unwrap_or_default().is_empty() => {
                return invalid("wait needs a selector or text")
            }
            "type" if selector.unwrap_or_default().is_empty() => return invalid("type needs a selector"),
            "type" if text.is_none() => return invalid("type needs a text"),
            "press" if key.unwrap_or_default().is_empty() => return invalid("press needs a key"),
            "select" if selector.unwrap_or_default().is_empty() => return invalid("select needs a selector"),
            "select" if value.is_none() => return invalid("select needs a value"),
            "extract" if expression.unwrap_or_default().is_empty() => {
                return invalid("extract needs an expression")
            }
            "upload" if selector.unwrap_or_default().is_empty() => return invalid("upload needs a selector"),
            "upload" if path_arg.unwrap_or_default().is_empty() => return invalid("upload needs a path"),
            "tab_open" if !url.is_some_and(allowed_url) => {
                return invalid("tab_open needs a url with an http://, https:// or data: scheme")
            }
            "tab_close" if tab_id.unwrap_or_default().is_empty() => return invalid("tab_close needs a tabId"),
            "tab_switch" if tab_id.unwrap_or_default().is_empty() => return invalid("tab_switch needs a tabId"),
            "text" | "screenshot" | "download" | "tab_list" => {}
            "open" | "click" | "eval" | "wait" | "type" | "press" | "select" | "extract" | "upload" | "tab_open" => {}
            other => return invalid(&format!("unknown browser action {other:?}")),
        }

        let tmp = paths::harness_tmp_dir(&wd);
        // Path safety runs before any launch: bad paths never start Chrome.
        let upload_file = match action {
            "upload" => match resolve_upload_path(path_arg.unwrap(), ctx, &wd) {
                Ok(file) => Some(file),
                Err(e) => return Ok(path_fail(e)),
            },
            _ => None,
        };
        let download_dir = match action {
            "download" => match resolve_download_dir(path_arg, ctx, &wd) {
                Ok(dir) => Some(dir),
                Err(e) => return Ok(path_fail(e)),
            },
            _ => None,
        };
        if let Some(dir) = download_dir.as_ref() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
            }
        }
        // Raw bytes never travel through serde_json strings (lossy UTF-8).
        enum Outcome {
            Text(String),
            Json(Value),
            Png(Vec<u8>),
            Empty,
        }
        let run = |browser: &mut Browser| -> Result<Outcome, AppError> {
            match action {
                "open" => browser.navigate(url.unwrap()).map(|_| Outcome::Empty),
                "text" => browser.get_text().map(Outcome::Text),
                "screenshot" => browser.screenshot_png().map(Outcome::Png),
                "click" => browser.click(selector.unwrap()).map(|_| Outcome::Empty),
                "eval" => browser.eval(expression.unwrap()).map(Outcome::Text),
                "wait" => {
                    let found = browser.wait_for(
                        selector.filter(|s| !s.is_empty()),
                        text.filter(|t| !t.is_empty()),
                        timeout,
                    )?;
                    if !found {
                        return Err(AppError::Http(format!(
                            "wait timed out after {} ms waiting for selector/text",
                            timeout.as_millis()
                        )));
                    }
                    Ok(Outcome::Empty)
                }
                "type" => browser
                    .type_text(selector.unwrap(), text.unwrap())
                    .map(|_| Outcome::Empty),
                "press" => browser.press_key(key.unwrap()).map(|_| Outcome::Empty),
                "select" => browser
                    .select_value(selector.unwrap(), value.unwrap())
                    .map(|_| Outcome::Empty),
                "extract" => browser.eval_json(expression.unwrap()).map(Outcome::Json),
                "upload" => browser
                    .set_file_input(selector.unwrap(), upload_file.as_ref().expect("resolved above"))
                    .map(|_| Outcome::Empty),
                "download" => {
                    let dir = download_dir.as_ref().expect("resolved above");
                    let dir_names = |dir: &Path| -> Vec<String> {
                        std::fs::read_dir(dir)
                            .map(|rd| {
                                rd.filter_map(|e| e.ok())
                                    .map(|e| e.file_name().to_string_lossy().into_owned())
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let before = dir_names(dir);
                    browser.arm_download(dir)?;
                    if let Some(sel) = selector.filter(|s| !s.is_empty()) {
                        browser.click(sel)?;
                    }
                    let deadline = Instant::now() + DOWNLOAD_TIMEOUT;
                    loop {
                        if let Some(name) = cdp::find_new_file(&before, &dir_names(dir)) {
                            return Ok(Outcome::Json(json!({
                                "downloaded": dir.join(name).display().to_string()
                            })));
                        }
                        if Instant::now() >= deadline {
                            return Err(AppError::Http(format!(
                                "download timed out after {} s: no new file appeared in {}",
                                DOWNLOAD_TIMEOUT.as_secs(),
                                dir.display()
                            )));
                        }
                        std::thread::sleep(DOWNLOAD_POLL);
                    }
                }
                "tab_open" => browser
                    .open_tab(url.unwrap())
                    .map(|id| Outcome::Json(json!({ "tabId": id }))),
                "tab_list" => browser.list_tabs().map(|tabs| Outcome::Json(json!({ "tabs": tabs }))),
                "tab_close" => {
                    browser.close_tab(tab_id.unwrap())?;
                    Ok(Outcome::Json(json!({
                        "closed": tab_id,
                        "active": browser.active_target_id()
                    })))
                }
                "tab_switch" => {
                    browser.switch_tab(tab_id.unwrap())?;
                    Ok(Outcome::Json(json!({
                        "switched": tab_id,
                        "active": browser.active_target_id()
                    })))
                }
                _ => unreachable!("action validated above"),
            }
        };

        let outcome = RUNTIME.with_session(
            &wd,
            headless,
            || launch_browser(&tmp, headless),
            |session| {
                session.last_used = Instant::now();
                run(&mut session.browser)
            },
        );

        let outcome = match outcome {
            Ok(value) => value,
            Err(message) => {
                return Ok(ToolResult::err(ToolErrorCode::ExecutionError, message));
            }
        };

        let data = match (action, outcome) {
            ("open", _) => json!({ "opened": url, "headless": headless }),
            ("text", Outcome::Text(text)) => {
                let (text, truncated) = truncate_text(&text);
                json!({ "text": text, "truncated": truncated })
            }
            ("screenshot", Outcome::Png(png)) => {
                let name = format!(
                    "browser-screenshot-{}.png",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or_default()
                );
                let file = tmp.join(name);
                if let Err(e) = std::fs::write(&file, &png) {
                    return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
                }
                json!({ "path": file.display().to_string(), "bytes": png.len() })
            }
            ("click", _) => json!({ "clicked": selector }),
            ("eval", Outcome::Text(result)) => json!({ "result": result }),
            ("wait", _) => json!({ "waited": true }),
            ("type", _) => json!({ "typed": selector, "chars": text.unwrap_or_default().chars().count() }),
            ("press", _) => json!({ "pressed": key }),
            ("select", _) => json!({ "selected": selector, "value": value }),
            ("extract", Outcome::Json(extracted)) => cap_json(extracted, max_chars),
            ("upload", _) => json!({
                "uploaded": selector,
                "path": upload_file.expect("resolved above").display().to_string()
            }),
            ("download", Outcome::Json(data)) => data,
            ("tab_open", Outcome::Json(data)) => data,
            ("tab_list", Outcome::Json(data)) => data,
            ("tab_close", Outcome::Json(data)) => data,
            ("tab_switch", Outcome::Json(data)) => data,
            _ => unreachable!("action validated above"),
        };
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
}

// Upload source: project mode resolves strictly inside the project (the
// fs_ops resolver rejects escapes, symlink escapes and absolute paths);
// harness mode uses the existing resolve_path semantics and just requires
// the file to exist.
fn resolve_upload_path(raw: &str, ctx: &ToolCtx, wd: &Path) -> crate::error::Result<PathBuf> {
    if let Some(project_id) = ctx.project_id {
        super::project_tools::with_conn(ctx, |conn| {
            crate::fs_ops::resolve_existing(conn, wd, project_id, raw)
        })
    } else {
        let file = super::harness_tools::canonicalish(&super::harness_tools::resolve_path(raw, wd));
        if !file.is_file() {
            return Err(AppError::Invalid(format!(
                "upload file not found: {}",
                file.display()
            )));
        }
        Ok(file)
    }
}

// Download folder: the default is Harness/tmp/downloads; an explicit one
// must stay inside Harness/ (harness mode) or inside the project (project
// mode), symlink escapes included.
fn resolve_download_dir(raw: Option<&str>, ctx: &ToolCtx, wd: &Path) -> crate::error::Result<PathBuf> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(paths::harness_tmp_dir(wd).join("downloads"));
    };
    let dir = super::harness_tools::canonicalish(&super::harness_tools::resolve_path(raw, wd));
    let (base, label) = match ctx.project_id {
        Some(project_id) => (
            super::project_tools::with_conn(ctx, |conn| {
                crate::projects::project_root(conn, wd, project_id)
            })?,
            "the project",
        ),
        None => (
            super::harness_tools::canonicalish(&paths::harness_dir(wd)),
            "Harness/",
        ),
    };
    if !dir.starts_with(&base) {
        return Err(AppError::Invalid(format!(
            "download path {raw:?} is not allowed: the download folder must stay inside {label}"
        )));
    }
    Ok(dir)
}

// Maps a path-resolution AppError to the envelope code (SPEC 6.4 style):
// containment and symlink refusals are permission_denied, missing files are
// not_found, configuration faults are execution_error.
fn path_fail(e: AppError) -> ToolResult {
    let msg = e.to_string();
    let code = if matches!(e, AppError::NotConfigured) || msg.starts_with("io error") {
        ToolErrorCode::ExecutionError
    } else if msg.contains("must stay inside")
        || msg.contains("escapes")
        || msg.contains("not project-relative")
        || msg.contains("symlink")
    {
        ToolErrorCode::PermissionDenied
    } else if msg.contains("not found") {
        ToolErrorCode::NotFound
    } else {
        ToolErrorCode::InvalidArgs
    };
    ToolResult::err(code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::{EnableOutcome, Resolution};
    use crate::agent::tool::validate_args;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use std::sync::Arc;

    fn ctx_for(wd: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cancel: None,
            project_id: None,
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.to_path_buf()),
            state: Some(Arc::new(AppState::new())),
            notes_root: None,
        }
    }

    #[test]
    fn browser_tool_policy_is_check_conditional_per_action() {
        let catalog = super::super::harness_tools::harness_catalog();
        match catalog.resolve("browser") {
            Resolution::Found(tool) => {
                assert_eq!(tool.class(), ToolClass::Mutating);
                assert_eq!(tool.policy(), PermissionPolicy::CheckConditional);
                for action in ["text", "screenshot", "tab_list", "extract", "wait"] {
                    assert!(
                        tool.danger_reason(&json!({ "action": action }), &ToolCtx::default())
                            .is_none(),
                        "{action} must not confirm"
                    );
                }
                for action in [
                    "open", "click", "type", "press", "select", "eval", "upload", "download",
                    "tab_open", "tab_close", "tab_switch",
                ] {
                    let reason = tool.danger_reason(&json!({ "action": action }), &ToolCtx::default());
                    assert!(reason.is_some(), "no danger_reason for {action}");
                    assert!(!reason.unwrap().is_empty());
                }
                // unknown and missing actions keep the confirmation
                assert_eq!(
                    tool.danger_reason(&json!({ "action": "weird" }), &ToolCtx::default())
                        .unwrap(),
                    "управление браузером"
                );
                assert!(tool
                    .danger_reason(&json!({}), &ToolCtx::default())
                    .is_some());
            }
            _ => panic!("browser tool missing from harness catalog"),
        }
    }

    #[test]
    fn browser_tool_is_enableable() {
        let mut catalog = super::super::harness_tools::harness_catalog();
        assert!(matches!(catalog.enable("browser"), EnableOutcome::Activated));
        assert!(catalog.to_specs().iter().any(|s| s.name == "browser"));
    }

    #[test]
    fn argument_schema_validates_actions_and_rejects_extras() {
        let schema = BrowserTool.parameters();
        for action in [
            "open", "text", "screenshot", "click", "eval", "wait", "type", "press", "select",
            "extract", "upload", "download", "tab_open", "tab_list", "tab_close", "tab_switch",
        ] {
            assert!(validate_args(&schema, &json!({ "action": action })).is_ok(), "{action}");
        }
        assert!(validate_args(&schema, &json!({ "action": "open", "url": "https://example.ru", "headless": false })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "wait", "selector": "#x", "text": "hi", "timeoutMs": 5000 })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "type", "selector": "#i", "text": "hello" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "press", "key": "Enter" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "select", "selector": "#s", "value": "b" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "extract", "expression": "document.title", "maxChars": 1000 })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "upload", "selector": "#f", "path": "Harness/a.bin" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "download" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "download", "selector": "a.dl", "path": "Harness/out" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "tab_open", "url": "https://example.ru" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "tab_close", "tabId": "T1" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "tab_switch", "tabId": "T1" })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "print" })).is_err());
        assert!(validate_args(&schema, &json!({})).is_err());
        assert!(validate_args(&schema, &json!({ "action": "text", "bogus": 1 })).is_err());
        assert!(validate_args(&schema, &json!({ "action": "text", "headless": "yes" })).is_err());
        assert!(validate_args(&schema, &json!({ "action": "wait", "timeoutMs": "5000" })).is_err());
        assert!(validate_args(&schema, &json!({ "action": "press", "key": true })).is_err());
        assert!(validate_args(&schema, &json!({ "action": "tab_close", "tabId": 5 })).is_err());
        assert!(validate_args(&schema, &json!({ "action": "upload", "selector": "#f", "path": 3 })).is_err());
    }

    #[test]
    fn missing_chrome_degrades_to_execution_error() {
        let wd = temp_test_dir("browser-missing");
        let ctx = ctx_for(&wd);
        // A machine without Chrome in the default locations (and no
        // CHROME_PATH) must get a clean error, not a panic.
        if cdp::find_chrome().is_ok() {
            eprintln!("chrome present on this machine; skipping strict assertion");
            return;
        }
        let r = BrowserTool.execute(&json!({ "action": "open", "url": "https://example.ru" }), &ctx).unwrap();
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("Chrome"), "{}", err.message);
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn invalid_args_are_rejected_before_any_launch() {
        let wd = temp_test_dir("browser-args");
        let ctx = ctx_for(&wd);
        for args in [
            json!({ "action": "open" }),
            json!({ "action": "open", "url": "file:///etc/passwd" }),
            json!({ "action": "click" }),
            json!({ "action": "eval" }),
            json!({ "action": "print" }),
            json!({ "action": "wait" }),
            json!({ "action": "wait", "selector": "" }),
            json!({ "action": "type" }),
            json!({ "action": "type", "selector": "#i" }),
            json!({ "action": "type", "text": "hi" }),
            json!({ "action": "press" }),
            json!({ "action": "select", "selector": "#s" }),
            json!({ "action": "extract" }),
            json!({ "action": "upload" }),
            json!({ "action": "upload", "selector": "#f" }),
            json!({ "action": "upload", "path": "Harness/a.bin" }),
            json!({ "action": "tab_open" }),
            json!({ "action": "tab_open", "url": "file:///etc/passwd" }),
            json!({ "action": "tab_close" }),
            json!({ "action": "tab_switch" }),
        ] {
            let r = BrowserTool.execute(&args, &ctx).unwrap();
            assert_eq!(r.error.unwrap().code, "invalid_args", "{args}");
        }
        let r = BrowserTool.execute(&json!({ "action": "text" }), &ToolCtx::default()).unwrap();
        assert_eq!(r.error.unwrap().code, "execution_error");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn url_allowlist_and_text_truncation_are_pure() {
        assert!(allowed_url("https://example.ru"));
        assert!(allowed_url("http://example.ru"));
        assert!(allowed_url("data:text/html,hi"));
        assert!(!allowed_url("file:///etc/passwd"));
        assert!(!allowed_url("javascript:alert(1)"));
        let (text, truncated) = truncate_text(&"х".repeat(TEXT_CAP + 5));
        assert!(truncated);
        assert_eq!(text.chars().count(), TEXT_CAP);
        let (text, truncated) = truncate_text("short");
        assert!(!truncated && text == "short");
    }

    #[test]
    fn wait_timeout_and_extract_cap_are_clamped() {
        assert_eq!(clamp_timeout_ms(None), Duration::from_secs(10));
        assert_eq!(clamp_timeout_ms(Some(500)), Duration::from_millis(500));
        assert_eq!(clamp_timeout_ms(Some(120_000)), Duration::from_millis(60_000));
        assert_eq!(clamp_timeout_ms(Some(-5)), Duration::from_millis(1));
        assert_eq!(clamp_max_chars(None), EXTRACT_CAP);
        assert_eq!(clamp_max_chars(Some(10)), 10);
        assert_eq!(clamp_max_chars(Some(0)), 1);
        assert_eq!(clamp_max_chars(Some(1_000_000)), EXTRACT_CAP);
    }

    #[test]
    fn cap_json_keeps_small_values_and_cuts_large_ones() {
        let small = cap_json(json!({ "a": 1 }), 100);
        assert_eq!(small["truncated"], false);
        assert_eq!(small["value"], json!({ "a": 1 }));
        let big = cap_json(json!("x".repeat(50)), 10);
        assert_eq!(big["truncated"], true);
        assert_eq!(big["value"].as_str().unwrap().chars().count(), 10);
        // multibyte text must be cut on char boundaries, never mid-UTF-8
        let big = cap_json(json!("х".repeat(50)), 10);
        assert_eq!(big["value"].as_str().unwrap().chars().count(), 10);
        let nullish = cap_json(Value::Null, 100);
        assert_eq!(nullish["truncated"], false);
        assert!(nullish["value"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn download_dir_must_stay_inside_harness_in_harness_mode() {
        let wd = temp_test_dir("browser-dirs");
        std::fs::create_dir_all(wd.join("Harness")).unwrap();
        let ctx = ctx_for(&wd);
        assert_eq!(
            resolve_download_dir(None, &ctx, &wd).unwrap(),
            paths::harness_tmp_dir(&wd).join("downloads")
        );
        assert!(resolve_download_dir(Some("Harness/out"), &ctx, &wd).is_ok());
        for bad in ["/tmp", "Harness/../../etc", "~/outside"] {
            let err = resolve_download_dir(Some(bad), &ctx, &wd).unwrap_err().to_string();
            assert!(err.contains("must stay inside Harness/"), "{bad}: {err}");
        }
        // a symlinked download dir escaping Harness is refused
        let outside = temp_test_dir("browser-dirs-outside");
        std::os::unix::fs::symlink(&outside, wd.join("Harness").join("link")).unwrap();
        let err = resolve_download_dir(Some("Harness/link"), &ctx, &wd)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must stay inside Harness/"), "{err}");
        std::fs::remove_dir_all(&outside).ok();
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn upload_path_must_exist_in_harness_mode() {
        let wd = temp_test_dir("browser-upload");
        std::fs::create_dir_all(wd.join("Harness")).unwrap();
        let ctx = ctx_for(&wd);
        let err = resolve_upload_path("Harness/nope.bin", &ctx, &wd)
            .unwrap_err()
            .to_string();
        assert!(err.contains("upload file not found"), "{err}");
        let file = wd.join("Harness").join("ok.bin");
        std::fs::write(&file, "x").unwrap();
        let resolved = resolve_upload_path("Harness/ok.bin", &ctx, &wd).unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn path_errors_map_to_clear_envelope_codes_before_any_launch() {
        let wd = temp_test_dir("browser-pathfail");
        std::fs::create_dir_all(wd.join("Harness")).unwrap();
        let ctx = ctx_for(&wd);
        let r = BrowserTool
            .execute(&json!({ "action": "download", "path": "/tmp" }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "permission_denied");
        let r = BrowserTool
            .execute(
                &json!({ "action": "upload", "selector": "#f", "path": "Harness/nope.bin" }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    // Manual end-to-end: needs a real Chrome on this machine.
    // Run with: cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_chrome_navigate_text_click_screenshot() {
        let chrome = cdp::find_chrome().expect("no Chrome installed");
        let wd = temp_test_dir("browser-live");
        let tmp = paths::harness_tmp_dir(&wd);
        let url = "data:text/html,%3Cbody%3Ehello%20cdp%3Cbutton%20id%3D%22b%22%20onclick%3D%22document.body.textContent%3D%27clicked%27%22%3Ego%3C%2Fbutton%3E%3C%2Fbody%3E";
        let mut browser = cdp::Browser::launch(&chrome, &tmp, true).expect("launch failed");
        browser.navigate(url).expect("navigate failed");
        assert_eq!(browser.get_text().unwrap(), "hello cdpgo");
        browser.click("#b").expect("click failed");
        assert_eq!(browser.get_text().unwrap(), "clicked");
        assert_eq!(browser.eval("1 + 2").unwrap(), "3");
        let png = browser.screenshot_png().unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let ctx = ctx_for(&wd);
        let r = BrowserTool.execute(&json!({ "action": "screenshot" }), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let path = r.data.unwrap()["path"].as_str().unwrap().to_owned();
        assert!(path.starts_with(&tmp.display().to_string()));
        assert!(std::fs::metadata(&path).unwrap().len() > 100);
        // Browser::drop kills the child; the tool session holds a second
        // Chrome that must be dropped before the temp dir can go away.
        drop(browser);
        RUNTIME.shutdown();
        // Chrome helper processes flush the profile a moment after the main
        // process dies, so the removal gets a few retries.
        for attempt in 0..10 {
            match std::fs::remove_dir_all(&wd) {
                Ok(()) => break,
                Err(e) if attempt == 9 => panic!("cleanup failed: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(300)),
            }
        }
    }

    // Manual end-to-end for the B1 actions: needs a real Chrome.
    // Run with: cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_chrome_wait_type_press_select_extract() {
        let chrome = cdp::find_chrome().expect("no Chrome installed");
        let wd = temp_test_dir("browser-live-b1");
        let tmp = paths::harness_tmp_dir(&wd);
        let url = "data:text/html,%3Cselect%20id%3D%22s%22%3E%3Coption%20value%3D%22a%22%3EA%3C%2Foption%3E%3Coption%20value%3D%22b%22%3EB%3C%2Foption%3E%3C%2Fselect%3E%3Cinput%20id%3D%22i%22%3E";
        let mut browser = cdp::Browser::launch(&chrome, &tmp, true).expect("launch failed");
        browser.navigate(url).expect("navigate failed");
        assert!(browser.wait_for(Some("#s"), Some("A"), Duration::from_secs(2)).unwrap());
        assert!(!browser.wait_for(Some("#missing"), None, Duration::from_millis(300)).unwrap());
        browser.type_text("#i", "hello").expect("type failed");
        assert_eq!(browser.eval("document.querySelector('#i').value").unwrap(), "hello");
        browser.select_value("#s", "b").expect("select failed");
        assert_eq!(browser.eval("document.querySelector('#s').value").unwrap(), "b");
        assert!(browser.select_value("#s", "zzz").is_err());
        assert_eq!(
            browser.eval_json("({ ok: true, n: 2 })").unwrap(),
            json!({ "ok": true, "n": 2 })
        );
        browser.press_key("Escape").expect("press failed");
        browser.press_key("a").expect("press char failed");
        assert!(browser.press_key("F13").is_err());
        drop(browser);
        RUNTIME.shutdown();
        for attempt in 0..10 {
            match std::fs::remove_dir_all(&wd) {
                Ok(()) => break,
                Err(e) if attempt == 9 => panic!("cleanup failed: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(300)),
            }
        }
    }

    // Manual end-to-end for the B2 actions: needs a real Chrome.
    // Run with: cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_chrome_upload_download_tabs() {
        let chrome = cdp::find_chrome().expect("no Chrome installed");
        let wd = temp_test_dir("browser-live-b2");
        let tmp = paths::harness_tmp_dir(&wd);
        let src = wd.join("upload-src.txt");
        std::fs::write(&src, "hello upload").unwrap();
        let url = "data:text/html,%3Cinput%20id%3D%22f%22%20type%3D%22file%22%3E%3Ca%20id%3D%22dl%22%20href%3D%22data%3Atext%2Fplain%2Cdl%22%20download%3D%22dl.txt%22%3Edl%3C%2Fa%3E";
        let mut browser = cdp::Browser::launch(&chrome, &tmp, true).expect("launch failed");
        browser.navigate(url).expect("navigate failed");
        // upload into the file input
        browser.set_file_input("#f", &src).expect("upload failed");
        assert_eq!(
            browser.eval("document.querySelector('#f').files.length").unwrap(),
            "1"
        );
        assert!(browser.set_file_input("#missing", &src).is_err());
        // tabs: open switches to the new page, list sees both pages
        let first = browser.active_target_id().to_string();
        let second = browser.open_tab("about:blank").expect("tab_open failed");
        assert_eq!(browser.active_target_id(), second);
        let tabs = browser.list_tabs().expect("tab_list failed");
        assert_eq!(tabs.len(), 2);
        assert!(tabs.iter().any(|t| t["id"] == first) && tabs.iter().any(|t| t["id"] == second));
        browser.switch_tab(&first).expect("switch failed");
        assert_eq!(browser.active_target_id(), first);
        assert!(browser.switch_tab("no-such-target").is_err());
        // closing the active tab re-attaches to a remaining page
        browser.switch_tab(&second).expect("switch back failed");
        browser.close_tab(&second).expect("close failed");
        assert_eq!(browser.active_target_id(), first);
        // download: arm, click the download link, poll the folder
        let dir = tmp.join("downloads");
        std::fs::create_dir_all(&dir).unwrap();
        browser.arm_download(&dir).expect("arm failed");
        browser.click("#dl").expect("click failed");
        let deadline = Instant::now() + Duration::from_secs(15);
        let names = |dir: &std::path::Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default()
        };
        let mut downloaded = None;
        while downloaded.is_none() && Instant::now() < deadline {
            downloaded = cdp::find_new_file(&[], &names(&dir));
            if downloaded.is_none() {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        let downloaded = downloaded.expect("download never landed");
        assert_eq!(downloaded, "dl.txt");
        drop(browser);
        RUNTIME.shutdown();
        for attempt in 0..10 {
            match std::fs::remove_dir_all(&wd) {
                Ok(()) => break,
                Err(e) if attempt == 9 => panic!("cleanup failed: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(300)),
            }
        }
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeBrowser {
        id: usize,
        kills: Arc<AtomicUsize>,
    }

    impl Drop for FakeBrowser {
        fn drop(&mut self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeLauncher {
        launches: AtomicUsize,
        kills: Arc<AtomicUsize>,
        fail: AtomicBool,
        delay: Duration,
    }

    impl FakeLauncher {
        fn new() -> Self {
            Self {
                launches: AtomicUsize::new(0),
                kills: Arc::new(AtomicUsize::new(0)),
                fail: AtomicBool::new(false),
                delay: Duration::ZERO,
            }
        }

        fn launch(&self) -> std::result::Result<FakeBrowser, String> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let id = self.launches.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err("fake launch failed".to_string());
            }
            Ok(FakeBrowser {
                id,
                kills: self.kills.clone(),
            })
        }
    }

    #[test]
    fn with_session_reuses_the_live_session() {
        let rt = BrowserRuntime::<FakeBrowser>::new();
        let launcher = FakeLauncher::new();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(
                rt.with_session(Path::new("/wd"), true, || launcher.launch(), |s| {
                    Ok(s.browser.id)
                })
                .unwrap(),
            );
        }
        assert_eq!(ids, vec![0, 0, 0]);
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn kill_clears_the_slot_and_the_next_call_relaunches() {
        let rt = BrowserRuntime::<FakeBrowser>::new();
        let launcher = FakeLauncher::new();
        let first = rt
            .with_session(Path::new("/wd"), true, || launcher.launch(), |s| {
                Ok(s.browser.id)
            })
            .unwrap();
        rt.kill();
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
        let second = rt
            .with_session(Path::new("/wd"), true, || launcher.launch(), |s| {
                Ok(s.browser.id)
            })
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn shutdown_kills_once_and_is_idempotent() {
        let rt = BrowserRuntime::<FakeBrowser>::new();
        let launcher = FakeLauncher::new();
        rt.with_session(Path::new("/wd"), true, || launcher.launch(), |_| Ok(()))
            .unwrap();
        rt.shutdown();
        rt.shutdown();
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_with_session_launches_exactly_once() {
        let rt = Arc::new(BrowserRuntime::<FakeBrowser>::new());
        let launcher = Arc::new(FakeLauncher {
            launches: AtomicUsize::new(0),
            kills: Arc::new(AtomicUsize::new(0)),
            fail: AtomicBool::new(false),
            delay: Duration::from_millis(30),
        });
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let rt = rt.clone();
                let launcher = launcher.clone();
                std::thread::spawn(move || {
                    rt.with_session(Path::new("/wd"), true, move || launcher.launch(), |s| {
                        Ok(s.browser.id)
                    })
                    .unwrap()
                })
            })
            .collect();
        let ids: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    }

    #[test]
    fn failed_launch_leaves_no_half_session() {
        let rt = BrowserRuntime::<FakeBrowser>::new();
        let launcher = FakeLauncher::new();
        launcher.fail.store(true, Ordering::SeqCst);
        let err = rt
            .with_session(Path::new("/wd"), true, || launcher.launch(), |_| Ok(()))
            .unwrap_err();
        assert_eq!(err, "fake launch failed");
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 0);
        launcher.fail.store(false, Ordering::SeqCst);
        let id = rt
            .with_session(Path::new("/wd"), true, || launcher.launch(), |s| {
                Ok(s.browser.id)
            })
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn action_failure_drops_the_session_and_the_next_call_relaunches() {
        let rt = BrowserRuntime::<FakeBrowser>::new();
        let launcher = FakeLauncher::new();
        let err = rt
            .with_session(Path::new("/wd"), true, || launcher.launch(), |_| -> crate::error::Result<()> {
                Err(AppError::Http("cdp died".into()))
            })
            .unwrap_err();
        assert_eq!(err, "http error: cdp died");
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
        let id = rt
            .with_session(Path::new("/wd"), true, || launcher.launch(), |s| {
                Ok(s.browser.id)
            })
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn changed_wd_or_headless_replaces_the_session() {
        let rt = BrowserRuntime::<FakeBrowser>::new();
        let launcher = FakeLauncher::new();
        rt.with_session(Path::new("/a"), true, || launcher.launch(), |_| Ok(()))
            .unwrap();
        rt.with_session(Path::new("/b"), true, || launcher.launch(), |_| Ok(()))
            .unwrap();
        rt.with_session(Path::new("/b"), false, || launcher.launch(), |_| Ok(()))
            .unwrap();
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 3);
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 2);
    }
}
