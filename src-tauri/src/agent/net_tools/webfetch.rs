// webfetch: GET an http(s) page and return its readable plain text. HTML is
// reduced by the html module, JSON and text pass through, everything else is
// refused; the download is capped at 2 MiB and the text at max_chars.
use super::fetch::{fetch, is_html, is_pass_through, CONNECT_TIMEOUT, READ_TIMEOUT};
use super::html::html_to_text;
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::mcp::check_url;
use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_MAX_CHARS: usize = 200_000;

pub struct WebfetchTool {
    connect: Duration,
    read: Duration,
}

impl WebfetchTool {
    pub fn new() -> Self {
        WebfetchTool {
            connect: CONNECT_TIMEOUT,
            read: READ_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub fn with_timeouts(connect: Duration, read: Duration) -> Self {
        WebfetchTool { connect, read }
    }
}

impl Default for WebfetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebfetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn description(&self) -> &str {
        "Fetch an http(s) page and return its readable plain text (HTML tags, scripts and styles are stripped; JSON and text pass through). Follows redirects, caps the download at 2 MiB and the text at max_chars, and refuses hosts on the SSRF deny list."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "max_chars": { "type": "number" }
            },
            "required": ["url"],
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
        30_000
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let url = args["url"].as_str().unwrap_or_default().trim();
        if url.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "url is empty"));
        }
        if let Err(e) = check_url(url) {
            return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, e.to_string()));
        }
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_MAX_CHARS as f64)
            .clamp(1.0, MAX_MAX_CHARS as f64) as usize;
        let fetched = match fetch(url, self.connect, self.read) {
            Ok(fetched) => fetched,
            // a redirect hop onto a denied host keeps the same verdict as
            // the initial URL check
            Err(e) if e.contains("ssrf") => {
                return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, e))
            }
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        let raw = String::from_utf8_lossy(&fetched.body).into_owned();
        let text = if is_html(&fetched.content_type) {
            html_to_text(&raw)
        } else if is_pass_through(&fetched.content_type) {
            raw
        } else {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!(
                    "unsupported content type {:?}; expected html, json or text",
                    fetched.content_type
                ),
            ));
        };
        let mut truncated = fetched.truncated;
        let text: String = if text.chars().count() > max_chars {
            truncated = true;
            text.chars().take(max_chars).collect()
        } else {
            text
        };
        Ok(ToolResult::ok(json!({
            "url": url,
            "finalUrl": fetched.final_url,
            "contentType": fetched.content_type,
            "text": text,
            "truncated": truncated,
        }))
        .capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::mock_server::{respond, Handler, MockServer};
    use std::io::Write;
    use std::sync::Arc;
    use std::time::Instant;

    fn mock(handler: Handler) -> MockServer {
        MockServer::start(handler)
    }

    #[test]
    fn webfetch_refuses_denied_hosts_before_connecting() {
        let tool = WebfetchTool::new();
        let r = tool
            .execute(&json!({ "url": "http://169.254.169.254/latest/meta-data/" }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        assert!(r.error.as_ref().unwrap().message.contains("ssrf"));
    }

    #[test]
    fn webfetch_rejects_bad_urls_and_empty_urls() {
        let tool = WebfetchTool::new();
        let r = tool.execute(&json!({ "url": "ftp://example.com" }), &ToolCtx::default()).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        let r = tool.execute(&json!({ "url": "   " }), &ToolCtx::default()).unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
    }

    #[test]
    fn webfetch_follows_redirects_and_guards_every_hop() {
        let server = mock(Arc::new(
            |stream: &mut std::net::TcpStream, req| match req.path.as_str() {
                "/start" => respond(stream, 302, "text/plain", "", &[("Location", "/final")]),
                "/to-metadata" => respond(
                    stream,
                    302,
                    "text/plain",
                    "",
                    &[("Location", "http://169.254.169.254/late")],
                ),
                _ => respond(stream, 200, "text/html; charset=utf-8", "<html><body><h1>Final</h1><p>body   text</p></body></html>", &[]),
            },
        ));

        let tool = WebfetchTool::new();
        let r = tool
            .execute(&json!({ "url": format!("{}/start", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["finalUrl"], format!("{}/final", server.url_base));
        assert_eq!(data["text"], "Final\nbody text");
        assert_eq!(data["truncated"], false);

        // a redirect hop to a denied host is refused, never fetched
        let r = tool
            .execute(&json!({ "url": format!("{}/to-metadata", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        assert!(r.error.as_ref().unwrap().message.contains("ssrf"));
    }

    #[test]
    fn webfetch_caps_body_at_two_mebibytes_and_text_at_max_chars() {
        let big = "x".repeat(3 * 1024 * 1024);
        let server = mock(Arc::new(move |stream: &mut std::net::TcpStream, _req| {
            // No set_nonblocking(false) workaround here: MockServer already
            // forces accepted sockets to blocking mode with I/O timeouts,
            // so the multi-MB write_all below is a plain blocking write.
            respond(stream, 200, "text/plain", &big, &[])
        }));
        let r = WebfetchTool::new()
            .execute(&json!({ "url": format!("{}/big", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["truncated"], true);
        assert_eq!(data["text"].as_str().unwrap().chars().count(), DEFAULT_MAX_CHARS);

        // explicit max_chars is honored; a request beyond the tool's
        // max_result_len comes back as limit_exceeded from capped()
        let r = WebfetchTool::new()
            .execute(
                &json!({ "url": format!("{}/big", server.url_base), "max_chars": 100 }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert_eq!(r.data.unwrap()["text"].as_str().unwrap().chars().count(), 100);
        let r = WebfetchTool::new()
            .execute(
                &json!({ "url": format!("{}/big", server.url_base), "max_chars": 1_000_000 }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "limit_exceeded");
    }

    #[test]
    fn webfetch_rejects_binary_content_types() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "image/png", "not really png", &[])
        }));
        let r = WebfetchTool::new()
            .execute(&json!({ "url": format!("{}/img", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "execution_error");
        assert!(r.error.as_ref().unwrap().message.contains("unsupported content type"));
    }

    #[test]
    fn webfetch_passes_json_through_without_conversion() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "application/json", "{\"a\": 1}", &[])
        }));
        let r = WebfetchTool::new()
            .execute(&json!({ "url": format!("{}/api", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["text"], "{\"a\": 1}");
    }

    #[test]
    fn webfetch_read_timeout_surfaces_as_execution_error() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\n")?;
            std::thread::sleep(Duration::from_secs(2));
            stream.write_all(b"never arrives in time")
        }));
        let tool = WebfetchTool::with_timeouts(Duration::from_millis(500), Duration::from_millis(300));
        let started = Instant::now();
        let r = tool
            .execute(&json!({ "url": format!("{}/slow", server.url_base) }), &ToolCtx::default())
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "execution_error");
        assert!(r.error.as_ref().unwrap().message.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    #[ignore = "live network: fetch example.com"]
    fn live_fetch_example_com() {
        let r = WebfetchTool::new()
            .execute(&json!({ "url": "https://example.com/" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert!(data["text"].as_str().unwrap().contains("example"));
    }
}
