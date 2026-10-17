// Harness browser tool (SPEC 5.2 #3): drives the installed Chrome/Chromium
// through the cdp module. The browser session survives across tool calls in
// one app process (open, then text/click/screenshot/eval on the same page)
// and is killed after IDLE_SECS of inactivity or when the tool errors. All
// artifacts (profile, screenshots) stay under Harness/tmp/. Missing Chrome is
// a plain execution_error: the tool is always registered, it just degrades.
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::cdp::{self, Browser};
use crate::error::AppError;
use crate::paths;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const IDLE_SECS: u64 = 120;
const TEXT_CAP: usize = 20_000;

struct Session {
    browser: Browser,
    wd: PathBuf,
    headless: bool,
    last_used: Instant,
}

// Single-user desktop app: one shared browser session for the whole process.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

#[cfg(test)]
fn end_session() {
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

fn danger_for(action: &str) -> &'static str {
    match action {
        "open" => "открыть страницу в браузере",
        "text" => "прочитать текст страницы через браузер",
        "screenshot" => "сделать скриншот страницы",
        "click" => "клик по элементу на странице",
        "eval" => "выполнить JavaScript на странице",
        _ => "управление браузером",
    }
}

pub struct BrowserTool;

impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }
    fn description(&self) -> &str {
        "Drive the installed Chrome/Chromium via CDP (headless by default, headful with headless:false). Actions: open (url), text, screenshot, click (selector), eval (expression). The page stays open between calls; the browser is killed after 120 s of idle. Screenshots are saved under Harness/tmp/. Requires confirmation."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["open", "text", "screenshot", "click", "eval"] },
                "url": { "type": "string" },
                "selector": { "type": "string" },
                "expression": { "type": "string" },
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
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some(danger_for(args.get("action").and_then(Value::as_str).unwrap_or("")).to_string())
    }
    fn max_result_len(&self) -> usize {
        30_000
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
        let headless = args.get("headless").and_then(Value::as_bool).unwrap_or(true);

        let invalid = |msg: &str| Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg));
        match action {
            "open" if !url.is_some_and(allowed_url) => {
                return invalid("open needs a url with an http://, https:// or data: scheme")
            }
            "click" if selector.unwrap_or_default().is_empty() => return invalid("click needs a selector"),
            "eval" if expression.unwrap_or_default().is_empty() => return invalid("eval needs an expression"),
            "text" | "screenshot" => {}
            "open" | "click" | "eval" => {}
            other => return invalid(&format!("unknown browser action {other:?}")),
        }

        let tmp = paths::harness_tmp_dir(&wd);
        let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
        let mut fresh = match guard.as_mut() {
            None => true,
            Some(session) => {
                let stale = session.wd != wd
                    || session.headless != headless
                    || session.last_used.elapsed() > Duration::from_secs(IDLE_SECS);
                if stale {
                    *guard = None; // Drop kills the old Chrome
                }
                stale
            }
        };
        // Raw bytes never travel through serde_json strings (lossy UTF-8).
        enum Outcome {
            Text(String),
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
                _ => unreachable!("action validated above"),
            }
        };

        let outcome: Result<Outcome, AppError> = loop {
            if fresh {
                let chrome = match cdp::find_chrome() {
                    Ok(chrome) => chrome,
                    Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string())),
                };
                if let Err(e) = std::fs::create_dir_all(&tmp) {
                    return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
                }
                match Browser::launch(&chrome, &tmp, headless) {
                    Ok(browser) => {
                        *guard = Some(Session {
                            browser,
                            wd: wd.clone(),
                            headless,
                            last_used: Instant::now(),
                        });
                        fresh = false;
                        continue;
                    }
                    Err(e) => {
                        *guard = None;
                        break Err(e);
                    }
                }
            }
            let session = guard.as_mut().expect("session set before fresh=false");
            session.last_used = Instant::now();
            break run(&mut session.browser);
        };

        let outcome = match outcome {
            Ok(value) => value,
            Err(e) => {
                *guard = None; // any CDP failure drops (kills) the browser
                return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e.to_string()));
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
            _ => unreachable!("action validated above"),
        };
        Ok(ToolResult::ok(data).capped(self.max_result_len()))
    }
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
            project_id: None,
            session_id: None,
            working_dir: Some(wd.to_path_buf()),
            state: Some(Arc::new(AppState::new())),
            notes_root: None,
        }
    }

    #[test]
    fn browser_tool_is_registered_as_deferred_check_tool() {
        let catalog = super::super::harness_tools::harness_catalog();
        match catalog.resolve("browser") {
            Resolution::Found(tool) => {
                assert_eq!(tool.class(), ToolClass::Mutating);
                assert_eq!(tool.policy(), PermissionPolicy::Check);
                for action in ["open", "text", "screenshot", "click", "eval"] {
                    let reason = tool.danger_reason(&json!({ "action": action }), &ToolCtx::default());
                    assert!(reason.is_some(), "no danger_reason for {action}");
                    assert!(!reason.unwrap().is_empty());
                }
                assert_eq!(
                    tool.danger_reason(&json!({ "action": "weird" }), &ToolCtx::default())
                        .unwrap(),
                    "управление браузером"
                );
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
        for action in ["open", "text", "screenshot", "click", "eval"] {
            assert!(validate_args(&schema, &json!({ "action": action })).is_ok(), "{action}");
        }
        assert!(validate_args(&schema, &json!({ "action": "open", "url": "https://example.ru", "headless": false })).is_ok());
        assert!(validate_args(&schema, &json!({ "action": "print" })).is_err());
        assert!(validate_args(&schema, &json!({})).is_err());
        assert!(validate_args(&schema, &json!({ "action": "text", "bogus": 1 })).is_err());
        assert!(validate_args(&schema, &json!({ "action": "text", "headless": "yes" })).is_err());
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
        end_session();
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
}
