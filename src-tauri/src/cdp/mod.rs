// Minimal synchronous CDP bridge (SPEC 5.2 #3): launch the installed
// Chrome/Chromium with --remote-debugging-port=0, take the ws endpoint from
// its stderr ("DevTools listening on ws://…"), then speak JSON-RPC over a
// blocking tungstenite WebSocket. The Chrome process and its profile live
// under Harness/tmp (never the system /tmp); dropping the Browser kills the
// child. One pump thread owns the socket for both directions, so timeouts
// come from channel recv_timeout instead of tungstenite internals.
use crate::error::{AppError, Result};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const DEFAULT_CHROME_CANDIDATES: [&str; 3] = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
];

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

// CHROME_PATH wins over the common macOS install locations.
pub fn find_chrome() -> Result<PathBuf> {
    let override_path = std::env::var_os("CHROME_PATH").map(PathBuf::from);
    find_chrome_in(override_path.as_deref(), &DEFAULT_CHROME_CANDIDATES)
}

pub fn find_chrome_in(override_path: Option<&Path>, candidates: &[&str]) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if is_executable(path) {
            return Ok(path.to_path_buf());
        }
    }
    for candidate in candidates {
        let path = Path::new(candidate);
        if is_executable(path) {
            return Ok(path.to_path_buf());
        }
    }
    Err(AppError::Invalid(
        "no Chrome/Chromium binary found: install Google Chrome or set CHROME_PATH".into(),
    ))
}

// Pure framing helpers: one JSON-RPC request out, reply-or-event in.
pub fn encode_request(id: u64, method: &str, params: &Value, session_id: Option<&str>) -> String {
    let mut msg = json!({ "id": id, "method": method, "params": params });
    if let Some(session_id) = session_id {
        msg["sessionId"] = json!(session_id);
    }
    msg.to_string()
}

pub enum CdpFrame {
    Reply {
        id: u64,
        result: Option<Value>,
        error: Option<Value>,
    },
    Event { method: String },
}

pub fn decode_frame(text: &str) -> Result<CdpFrame> {
    let value: Value = serde_json::from_str(text)?;
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        return Ok(CdpFrame::Reply {
            id,
            result: value.get("result").cloned(),
            error: value.get("error").cloned(),
        });
    }
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        return Ok(CdpFrame::Event {
            method: method.to_string(),
        });
    }
    Err(AppError::Json(format!("unrecognized CDP frame: {text}")))
}

// "DevTools listening on ws://127.0.0.1:PORT/devtools/browser/TOKEN"
pub fn parse_devtools_url(line: &str) -> Option<String> {
    let marker = "DevTools listening on ";
    let start = line.find(marker)? + marker.len();
    let url = line[start..].trim();
    url.starts_with("ws://").then(|| url.to_string())
}

// Runtime.evaluate envelope: exception details beat a bare "undefined".
pub fn exception_text(exception: &Value) -> String {
    exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .and_then(Value::as_str)
        .or_else(|| exception.get("text").and_then(Value::as_str))
        .unwrap_or("unknown error")
        .to_string()
}

pub fn eval_text(result: &Value) -> Result<String> {
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(AppError::Invalid(format!(
            "page JS error: {}",
            exception_text(exception)
        )));
    }
    let value = &result["result"]["value"];
    if value.is_null() {
        return Err(AppError::Invalid("expression returned null/undefined".into()));
    }
    Ok(match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

// Pure request builders for the interaction actions: no I/O, so the exact
// CDP payloads are unit-testable without a live Chrome.
pub fn wait_expression(selector: Option<&str>, text: Option<&str>) -> String {
    let sel = selector.map(|s| format!("!!document.querySelector({s:?})"));
    let txt = text.map(|t| format!("document.body.innerText.includes({t:?})"));
    match (sel, txt) {
        (Some(s), Some(t)) => format!("({s}) && ({t})"),
        (Some(s), None) => s,
        (None, Some(t)) => t,
        (None, None) => "false".into(),
    }
}

pub fn focus_expression(selector: &str) -> String {
    format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return 'no-match'; el.focus(); return document.activeElement === el ? 'focused' : 'unfocusable'; }})()"
    )
}

// React-friendly fallback for elements that never take focus: write through
// the native value setter, then fire input+change like a real user would.
pub fn type_fallback_expression(selector: &str, text: &str) -> String {
    format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return 'no-match'; const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype; Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, {text:?}); el.dispatchEvent(new Event('input', {{ bubbles: true }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); return 'ok'; }})()"
    )
}

pub fn select_expression(selector: &str, value: &str) -> String {
    format!(
        "(() => {{ const el = document.querySelector({selector:?}); if (!el) return 'no-match'; if (el.tagName !== 'SELECT') return 'not-select'; el.value = {value:?}; if (el.value !== {value:?}) return 'no-option'; el.dispatchEvent(new Event('change', {{ bubbles: true }})); return 'ok'; }})()"
    )
}

pub fn insert_text_params(text: &str) -> Value {
    json!({ "text": text })
}

// (name, key, code, windowsVirtualKeyCode) for the keys the tool promises.
const NAMED_KEYS: [(&str, &str, &str, u32); 8] = [
    ("Enter", "Enter", "Enter", 13),
    ("Tab", "Tab", "Tab", 9),
    ("Escape", "Escape", "Escape", 27),
    ("Backspace", "Backspace", "Backspace", 8),
    ("ArrowUp", "ArrowUp", "ArrowUp", 38),
    ("ArrowDown", "ArrowDown", "ArrowDown", 40),
    ("ArrowLeft", "ArrowLeft", "ArrowLeft", 37),
    ("ArrowRight", "ArrowRight", "ArrowRight", 39),
];

// (keyDown params, keyUp params) for a named key or a single character.
pub fn key_params(key: &str) -> Option<(Value, Value)> {
    let (key_name, code, vk, text): (String, Option<String>, Option<u32>, Option<String>) =
        if let Some((_, name, code, vk)) = NAMED_KEYS.iter().find(|(n, ..)| n.eq_ignore_ascii_case(key)) {
            (name.to_string(), Some(code.to_string()), Some(*vk), None)
        } else {
            let mut chars = key.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if ch.is_ascii_alphabetic() {
                let upper = ch.to_ascii_uppercase();
                (ch.to_string(), Some(format!("Key{upper}")), Some(upper as u32), Some(ch.to_string()))
            } else if ch.is_ascii_digit() {
                (ch.to_string(), Some(format!("Digit{ch}")), Some(ch as u32), Some(ch.to_string()))
            } else if ch == ' ' {
                (" ".to_string(), Some("Space".to_string()), Some(32), Some(" ".to_string()))
            } else {
                let vk = if ch.is_ascii() { Some(ch as u32) } else { None };
                (ch.to_string(), None, vk, Some(ch.to_string()))
            }
        };
    let mut down = Map::new();
    down.insert("type".into(), json!("keyDown"));
    down.insert("key".into(), json!(key_name));
    if let Some(code) = &code {
        down.insert("code".into(), json!(code));
    }
    if let Some(vk) = vk {
        down.insert("windowsVirtualKeyCode".into(), json!(vk));
        down.insert("nativeVirtualKeyCode".into(), json!(vk));
    }
    if let Some(text) = &text {
        down.insert("text".into(), json!(text));
    }
    let mut up = down.clone();
    up.insert("type".into(), json!("keyUp"));
    up.remove("text");
    Some((Value::Object(down), Value::Object(up)))
}

pub struct Browser {
    child: Child,
    tx_out: Sender<String>,
    rx_in: Receiver<CdpFrame>,
    rx_err: Receiver<serde_json::Error>,
    next_id: u64,
    session_id: String,
    // stderr is drained forever by the launch thread into this channel; the
    // queue is kept so a failed call can report the browser's own complaints.
    stderr_lines: Receiver<String>,
    stderr_tail: Vec<String>,
    pump: Option<JoinHandle<()>>,
}

impl Browser {
    pub fn launch(chrome: &Path, tmp_dir: &Path, headless: bool) -> Result<Browser> {
        // Unique per launch: a killed Chrome can leave a stale SingletonLock
        // behind, and two live browsers must not share one profile. Leftover
        // profile folders are reaped by the Harness/tmp cleanup (SPEC 3.1).
        let profile = tmp_dir.join(format!(
            "chrome-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&profile)?;
        let mut args: Vec<String> = vec![
            "--remote-debugging-port=0".into(),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
        ];
        if headless {
            args.push("--headless=new".into());
        }
        let mut child = Command::new(chrome)
            .args(&args)
            .env("TMPDIR", tmp_dir)
            .env("TEMP", tmp_dir)
            .env("TMP", tmp_dir)
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Io("chrome stderr missing".into()))?;
        let (tx_line, rx_line) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = buf.trim_end_matches(['\r', '\n']);
                        if !line.is_empty() && tx_line.send(line.to_string()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut ws_url: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        while ws_url.is_none() {
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                break;
            }
            match rx_line.recv_timeout(timeout) {
                Ok(line) => {
                    if let Some(url) = parse_devtools_url(&line) {
                        ws_url = Some(url);
                    } else {
                        seen.push(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let Some(ws_url) = ws_url else {
            let status = child.try_wait().ok().flatten();
            let _ = child.kill();
            let _ = child.wait();
            let tail = seen.join("\n");
            return Err(AppError::Io(format!(
                "chrome did not report a DevTools endpoint within 15 s (exit {:?}):\n{tail}",
                status.map(|s| s.code().unwrap_or(-1))
            )));
        };
        let (mut ws, _response) = tungstenite::connect(&ws_url)
            .map_err(|e| AppError::Io(format!("cdp websocket connect failed: {e}")))?;
        // The pump owns the socket in both directions; the stream is
        // nonblocking so one loop can multiplex requests and frames without
        // ever deadlocking between two sequential requests.
        if let tungstenite::stream::MaybeTlsStream::Plain(plain) = ws.get_mut() {
            let _ = plain.set_nonblocking(true);
        }
        let (tx_out, rx_out) = mpsc::channel::<String>();
        let (tx_in, rx_in) = mpsc::channel::<CdpFrame>();
        let (tx_err, rx_err) = mpsc::channel::<serde_json::Error>();
        let pump = std::thread::spawn(move || {
            let mut ws = ws;
            'pump: loop {
                while let Ok(text) = rx_out.try_recv() {
                    if ws.send(tungstenite::Message::Text(text.into())).is_err() {
                        break 'pump;
                    }
                }
                loop {
                    match ws.read() {
                        Ok(tungstenite::Message::Text(text)) => {
                            match decode_frame(text.as_str()) {
                                Ok(frame) => {
                                    if tx_in.send(frame).is_err() {
                                        return;
                                    }
                                }
                                Err(e) => {
                                    let wrapped = serde_json::Error::io(std::io::Error::other(e.to_string()));
                                    if tx_err.send(wrapped).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e))
                            if e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Nothing readable right now: wait for the next
                            // request (or channel close) instead of spinning.
                            match rx_out.recv_timeout(Duration::from_millis(100)) {
                                Ok(text) => {
                                    if ws.send(tungstenite::Message::Text(text.into())).is_err() {
                                        break 'pump;
                                    }
                                    continue;
                                }
                                Err(RecvTimeoutError::Disconnected) => break 'pump,
                                Err(RecvTimeoutError::Timeout) => continue 'pump,
                            }
                        }
                        Err(_) => break 'pump,
                    }
                }
            }
        });
        let mut browser = Browser {
            child,
            tx_out,
            rx_in,
            rx_err,
            next_id: 0,
            session_id: String::new(),
            stderr_lines: rx_line,
            stderr_tail: seen,
            pump: Some(pump),
        };
        let target = browser.call("Target.createTarget", &json!({ "url": "about:blank" }), None)?;
        let target_id = target["targetId"]
            .as_str()
            .ok_or_else(|| AppError::Json("Target.createTarget returned no targetId".into()))?
            .to_string();
        let attached = browser.call(
            "Target.attachToTarget",
            &json!({ "targetId": target_id, "flatten": true }),
            None,
        )?;
        browser.session_id = attached["sessionId"]
            .as_str()
            .ok_or_else(|| AppError::Json("Target.attachToTarget returned no sessionId".into()))?
            .to_string();
        let sid = browser.session_id.clone();
        browser.call("Page.enable", &json!({}), Some(&sid))?;
        Ok(browser)
    }

    fn request(&mut self, text: String) -> Result<()> {
        // If the pump is gone the failure surfaces on the next recv as
        // Disconnected; a send error here is not fatal on its own.
        let _ = self.tx_out.send(text);
        Ok(())
    }

    fn parse_error(&mut self) -> Option<AppError> {
        self.rx_err.try_recv().ok().map(|e| AppError::Json(e.to_string()))
    }

    fn dead_error(&self) -> AppError {
        let tail = self.stderr_tail.last().cloned().unwrap_or_default();
        AppError::Io(format!("cdp connection closed; browser said: {tail}"))
    }

    fn drain_stderr(&mut self) {
        while let Ok(line) = self.stderr_lines.try_recv() {
            self.stderr_tail.push(line);
            if self.stderr_tail.len() > 50 {
                self.stderr_tail.remove(0);
            }
        }
    }

    fn call(&mut self, method: &str, params: &Value, session_id: Option<&str>) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.request(encode_request(id, method, params, session_id))?;
        loop {
            match self.rx_in.recv_timeout(Duration::from_secs(20)) {
                Ok(CdpFrame::Reply { id: got, result, error }) => {
                    if got != id {
                        continue;
                    }
                    if let Some(error) = error {
                        return Err(AppError::Invalid(format!(
                            "cdp {method} failed: {}",
                            error.get("message").and_then(Value::as_str).unwrap_or("unknown")
                        )));
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Ok(CdpFrame::Event { .. }) => {}
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(e) = self.parse_error() {
                        return Err(e);
                    }
                    return Err(AppError::Http(format!("cdp {method} timed out after 20 s")));
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.dead_error()),
            }
        }
    }

    fn wait_event(&mut self, method: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.rx_in.recv_timeout(Duration::from_millis(200)) {
                Ok(CdpFrame::Event { method: name, .. }) if name == method => return Ok(()),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err(self.dead_error()),
            }
            self.drain_stderr();
            if Instant::now() >= deadline {
                return Err(AppError::Http(format!(
                    "page event {method} not seen within {} s",
                    timeout.as_secs()
                )));
            }
        }
    }

    pub fn navigate(&mut self, url: &str) -> Result<()> {
        let session_id = self.session_id.clone();
        self.call("Page.navigate", &json!({ "url": url }), Some(&session_id))?;
        self.wait_event("Page.loadEventFired", Duration::from_secs(20))
    }

    pub fn get_text(&mut self) -> Result<String> {
        let session_id = self.session_id.clone();
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": "document.body.innerText", "returnByValue": true }),
            Some(&session_id),
        )?;
        eval_text(&result)
    }

    pub fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        let session_id = self.session_id.clone();
        let result = self.call("Page.captureScreenshot", &json!({ "format": "png" }), Some(&session_id))?;
        let data = result["data"]
            .as_str()
            .ok_or_else(|| AppError::Json("Page.captureScreenshot returned no data".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| AppError::Json(format!("bad screenshot base64: {e}")))
    }

    pub fn click(&mut self, selector: &str) -> Result<()> {
        let expression = format!(
            "(() => {{ const el = document.querySelector({selector:?}); if (!el) return 'no-match'; el.click(); return 'ok'; }})()"
        );
        let session_id = self.session_id.clone();
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": expression, "returnByValue": true }),
            Some(&session_id),
        )?;
        if eval_text(&result)? == "no-match" {
            return Err(AppError::Invalid(format!("no element matches selector {selector:?}")));
        }
        Ok(())
    }

    pub fn eval(&mut self, expression: &str) -> Result<String> {
        let session_id = self.session_id.clone();
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": expression, "returnByValue": true }),
            Some(&session_id),
        )?;
        eval_text(&result)
    }

    // Polls the condition every 200 ms; Ok(true) = seen, Ok(false) = timed out.
    pub fn wait_for(
        &mut self,
        selector: Option<&str>,
        text: Option<&str>,
        timeout: Duration,
    ) -> Result<bool> {
        let expression = wait_expression(selector, text);
        let session_id = self.session_id.clone();
        let deadline = Instant::now() + timeout;
        loop {
            let result = self.call(
                "Runtime.evaluate",
                &json!({ "expression": expression, "returnByValue": true }),
                Some(&session_id),
            )?;
            if let Some(exception) = result.get("exceptionDetails") {
                return Err(AppError::Invalid(format!(
                    "page JS error: {}",
                    exception_text(exception)
                )));
            }
            if result["result"]["value"].as_bool().unwrap_or(false) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn insert_text(&mut self, text: &str) -> Result<()> {
        let session_id = self.session_id.clone();
        self.call("Input.insertText", &insert_text_params(text), Some(&session_id))
            .map(|_| ())
    }

    pub fn press_key(&mut self, key: &str) -> Result<()> {
        let (down, up) = key_params(key).ok_or_else(|| {
            AppError::Invalid(format!(
                "unsupported key {key:?}: use Enter, Tab, Escape, Backspace, ArrowUp/Down/Left/Right or a single character"
            ))
        })?;
        let session_id = self.session_id.clone();
        self.call("Input.dispatchKeyEvent", &down, Some(&session_id))?;
        self.call("Input.dispatchKeyEvent", &up, Some(&session_id))
            .map(|_| ())
    }

    // Focus first (so Input.insertText lands like real typing); elements that
    // refuse focus get the native-setter fallback instead.
    pub fn type_text(&mut self, selector: &str, text: &str) -> Result<()> {
        let session_id = self.session_id.clone();
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": focus_expression(selector), "returnByValue": true }),
            Some(&session_id),
        )?;
        match eval_text(&result)?.as_str() {
            "no-match" => {
                return Err(AppError::Invalid(format!(
                    "no element matches selector {selector:?}"
                )))
            }
            "focused" => return self.insert_text(text),
            _ => {}
        }
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": type_fallback_expression(selector, text), "returnByValue": true }),
            Some(&session_id),
        )?;
        if eval_text(&result)? == "no-match" {
            return Err(AppError::Invalid(format!(
                "no element matches selector {selector:?}"
            )));
        }
        Ok(())
    }

    pub fn select_value(&mut self, selector: &str, value: &str) -> Result<()> {
        let session_id = self.session_id.clone();
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": select_expression(selector, value), "returnByValue": true }),
            Some(&session_id),
        )?;
        match eval_text(&result)?.as_str() {
            "ok" => Ok(()),
            "not-select" => Err(AppError::Invalid(format!(
                "element {selector:?} is not a <select>"
            ))),
            "no-option" => Err(AppError::Invalid(format!(
                "no <option> with value {value:?} in {selector:?}"
            ))),
            _ => Err(AppError::Invalid(format!(
                "no element matches selector {selector:?}"
            ))),
        }
    }

    // Like eval but keeps the JSON value (null included) instead of stringifying.
    pub fn eval_json(&mut self, expression: &str) -> Result<Value> {
        let session_id = self.session_id.clone();
        let result = self.call(
            "Runtime.evaluate",
            &json!({ "expression": expression, "returnByValue": true }),
            Some(&session_id),
        )?;
        if let Some(exception) = result.get("exceptionDetails") {
            return Err(AppError::Invalid(format!(
                "page JS error: {}",
                exception_text(exception)
            )));
        }
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Closing the request channel ends the pump thread; killing the child
        // closes the socket and the stderr pipe, so both threads finish.
        drop(std::mem::replace(&mut self.tx_out, mpsc::channel().0));
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_chrome_errors_gracefully_when_nothing_exists() {
        let err = find_chrome_in(None, &["/nonexistent/chrome", "/also/missing/chrome"]).unwrap_err();
        assert!(err.to_string().contains("no Chrome/Chromium binary found"));
    }

    #[test]
    fn find_chrome_honors_override_before_candidates() {
        let found = find_chrome_in(Some(Path::new("/bin/echo")), &["/nonexistent/chrome"]).unwrap();
        assert_eq!(found, PathBuf::from("/bin/echo"));
        // a non-executable override falls through to the candidates
        let err = find_chrome_in(Some(Path::new("/etc/hosts")), &["/nonexistent/chrome"]).unwrap_err();
        assert!(err.to_string().contains("no Chrome"));
    }

    #[test]
    fn encode_request_carries_id_params_and_session() {
        let text = encode_request(7, "Page.navigate", &json!({ "url": "https://example.ru" }), Some("S1"));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "Page.navigate");
        assert_eq!(value["sessionId"], "S1");
        assert_eq!(value["params"]["url"], "https://example.ru");
        let no_session: Value =
            serde_json::from_str(&encode_request(1, "Target.createTarget", &json!({}), None)).unwrap();
        assert!(no_session.get("sessionId").is_none());
    }

    #[test]
    fn decode_frame_separates_replies_from_events() {
        match decode_frame(r#"{"id":3,"result":{"data":"AAA"}}"#).unwrap() {
            CdpFrame::Reply { id, result, error } => {
                assert_eq!(id, 3);
                assert_eq!(result.unwrap()["data"], "AAA");
                assert!(error.is_none());
            }
            CdpFrame::Event { .. } => panic!("expected reply"),
        }
        match decode_frame(r#"{"method":"Page.loadEventFired","sessionId":"S1","params":{}}"#).unwrap() {
            CdpFrame::Event { method } => assert_eq!(method, "Page.loadEventFired"),
            CdpFrame::Reply { .. } => panic!("expected event"),
        }
        match decode_frame(r#"{"id":9,"error":{"code":-32000,"message":"boom"}}"#).unwrap() {
            CdpFrame::Reply { id, error, .. } => {
                assert_eq!(id, 9);
                assert_eq!(error.unwrap()["message"], "boom");
            }
            CdpFrame::Event { .. } => panic!("expected reply"),
        }
        assert!(decode_frame("not json").is_err());
        assert!(decode_frame(r#"{"something":1}"#).is_err());
    }

    #[test]
    fn id_correlation_matches_out_of_order_replies() {
        let first = encode_request(1, "Page.enable", &json!({}), Some("S1"));
        let second = encode_request(2, "Runtime.evaluate", &json!({ "expression": "1+1" }), Some("S1"));
        for text in [first, second] {
            let sent: Value = serde_json::from_str(&text).unwrap();
            let id = sent["id"].as_u64().unwrap();
            // replies arrive swapped: each caller must still get its own result
            let reply = json!({ "id": id, "result": { "for": id } });
            match decode_frame(&reply.to_string()).unwrap() {
                CdpFrame::Reply { id: got, result, .. } => {
                    assert_eq!(got, id);
                    assert_eq!(result.unwrap()["for"], id);
                }
                CdpFrame::Event { .. } => panic!("expected reply"),
            }
        }
    }

    #[test]
    fn parse_devtools_url_extracts_ws_endpoint() {
        let line = "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123";
        assert_eq!(
            parse_devtools_url(line).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-123")
        );
        assert!(parse_devtools_url("some other chrome output").is_none());
        assert!(parse_devtools_url("DevTools listening on http://127.0.0.1:9222").is_none());
    }

    #[test]
    fn eval_text_prefers_exception_details_and_stringifies_values() {
        let ok = json!({ "result": { "type": "string", "value": "hello" } });
        assert_eq!(eval_text(&ok).unwrap(), "hello");
        let number = json!({ "result": { "type": "number", "value": 42 } });
        assert_eq!(eval_text(&number).unwrap(), "42");
        let null = json!({ "result": { "type": "object", "subtype": "null", "value": null } });
        assert!(eval_text(&null).unwrap_err().to_string().contains("null/undefined"));
        let thrown = json!({
            "exceptionDetails": { "text": "Uncaught", "exception": { "description": "TypeError: x is undefined" } }
        });
        let err = eval_text(&thrown).unwrap_err();
        assert!(err.to_string().contains("TypeError: x is undefined"));
    }

    #[test]
    fn wait_expression_covers_selector_text_and_both() {
        let sel = wait_expression(Some("#b"), None);
        assert!(sel.contains(r##"!!document.querySelector("#b")"##), "{sel}");
        let txt = wait_expression(None, Some("hello world"));
        assert!(txt.contains(r##"document.body.innerText.includes("hello world")"##), "{txt}");
        let both = wait_expression(Some("#b"), Some("hi"));
        assert!(both.contains("&&") && both.contains("querySelector") && both.contains("includes"), "{both}");
        assert_eq!(wait_expression(None, None), "false");
    }

    #[test]
    fn key_params_maps_named_keys_and_single_chars() {
        let (down, up) = key_params("Enter").unwrap();
        assert_eq!(down["type"], "keyDown");
        assert_eq!(down["key"], "Enter");
        assert_eq!(down["code"], "Enter");
        assert_eq!(down["windowsVirtualKeyCode"], 13);
        assert_eq!(down["nativeVirtualKeyCode"], 13);
        assert_eq!(up["type"], "keyUp");
        assert!(up.get("text").is_none());
        let (down, _) = key_params("a").unwrap();
        assert_eq!(down["text"], "a");
        assert_eq!(down["code"], "KeyA");
        assert_eq!(down["windowsVirtualKeyCode"], 65);
        let (down, _) = key_params("ArrowDown").unwrap();
        assert_eq!(down["windowsVirtualKeyCode"], 40);
        let (down, _) = key_params("7").unwrap();
        assert_eq!(down["code"], "Digit7");
        assert_eq!(down["windowsVirtualKeyCode"], 55);
        assert!(key_params("F13").is_none());
        assert!(key_params("").is_none());
    }

    #[test]
    fn interaction_request_builders_are_pure() {
        let focus = focus_expression("#i");
        assert!(focus.contains(r##"document.querySelector("#i")"##), "{focus}");
        assert!(focus.contains("el.focus()"));
        let fallback = type_fallback_expression("#i", "hi");
        assert!(fallback.contains("getOwnPropertyDescriptor"));
        assert!(fallback.contains("'input'") && fallback.contains("'change'"));
        assert!(fallback.contains(r##""hi""##), "{fallback}");
        let select = select_expression("#s", "b");
        assert!(select.contains("tagName !== 'SELECT'"));
        assert!(select.contains("'change'"));
        assert!(select.contains(r##"el.value = "b""##), "{select}");
        assert_eq!(insert_text_params("hi"), json!({ "text": "hi" }));
    }
}
