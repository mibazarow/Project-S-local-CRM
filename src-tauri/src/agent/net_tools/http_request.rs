// http_request: the general-method twin of webfetch for API checks (webfetch
// is GET-only and rewrites HTML): any safe method with custom headers and a
// string body. Redirects are followed manually (max 5) with the SSRF guard on
// every hop; 301/302/303 re-issue the follow-up as a bodyless GET, 307/308
// repeat the method and body. Non-2xx statuses come back as data, not errors:
// for an API check the status and error body are exactly what the caller asked
// for. Textual content types (text/*, json, xml) are returned as UTF-8 text,
// everything else as base64, both capped at max_bytes.
use super::fetch::{is_pass_through, resolve_url, MAX_REDIRECTS};
use super::super::result::{ToolErrorCode, ToolResult};
use super::super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use base64::Engine as _;
use crate::error::Result;
use crate::mcp::check_url;
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
const MAX_TIMEOUT_SECS: f64 = 120.0;
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MAX_BYTES: u64 = 10 * 1024 * 1024;
const HTTP_METHODS: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

struct HttpResponse {
    status: u16,
    final_url: String,
    content_type: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    truncated: bool,
}

fn http_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    max_bytes: u64,
    timeout: Duration,
) -> std::result::Result<HttpResponse, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .redirects(0)
        .build();
    let mut method = method.to_string();
    let mut url = url.to_string();
    let mut body = body.map(str::to_string);
    for _ in 0..=MAX_REDIRECTS {
        check_url(&url).map_err(|e| e.to_string())?;
        let mut req = agent.request(&method, &url);
        for (name, value) in headers {
            req = req.set(name, value);
        }
        let resp = match body.as_deref() {
            Some(body) => req.send_string(body),
            None => req.call(),
        };
        // 4xx/5xx arrive as Error::Status: return the response itself, the
        // status and body are the data an API check is looking for.
        let resp = match resp {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(e) => return Err(format!("{method} request to {url} failed: {e}")),
        };
        let status = resp.status();
        if (300..400).contains(&status) {
            let Some(location) = resp.header("location") else {
                return Err(format!("redirect {status} from {url} has no location header"));
            };
            let Some(next) = resolve_url(&url, location.trim()) else {
                return Err(format!("unresolvable redirect target {location:?} from {url}"));
            };
            url = next;
            if status != 307 && status != 308 {
                method = "GET".to_string();
                body = None;
            }
            continue;
        }
        let content_type = resp.content_type().to_string();
        // ureq 2.12 has no header-map accessor: names come lowercased and
        // may repeat, so dedup them and join every value of a name.
        let mut header_names = resp.headers_names();
        header_names.sort();
        header_names.dedup();
        let headers = header_names
            .into_iter()
            .map(|name| {
                let value = resp.all(&name).join(", ");
                (name, value)
            })
            .collect();
        let mut buf = Vec::new();
        if let Err(e) = (&mut resp.into_reader()).take(max_bytes + 1).read_to_end(&mut buf) {
            return Err(format!("reading response body from {url} failed: {e}"));
        }
        let truncated = buf.len() as u64 > max_bytes;
        buf.truncate(max_bytes as usize);
        return Ok(HttpResponse {
            status,
            final_url: url,
            content_type,
            headers,
            body: buf,
            truncated,
        });
    }
    Err(format!("refused: more than {MAX_REDIRECTS} redirects"))
}

pub struct HttpRequestTool;

impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }
    fn description(&self) -> &str {
        "General HTTP request for API checks: GET/POST/PUT/PATCH/DELETE/HEAD, headers/body, timeoutSec, maxBytes; SSRF-guarded redirects; text/base64 body."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "method": { "type": "string" },
                "url": { "type": "string" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "type": "string" },
                "timeoutSec": { "type": "number" },
                "maxBytes": { "type": "number" }
            },
            "required": ["method", "url"],
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
        let method = args.get("method").and_then(Value::as_str)?.trim().to_uppercase();
        let url = args.get("url").and_then(Value::as_str)?;
        matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
            .then(|| format!("HTTP {method} {url}"))
    }
    fn max_result_len(&self) -> usize {
        30_000
    }
    fn execute(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolResult> {
        let method = args["method"].as_str().unwrap_or_default().trim().to_uppercase();
        if !HTTP_METHODS.contains(&method.as_str()) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("unsupported method {method:?}; expected GET, POST, PUT, PATCH, DELETE or HEAD"),
            ));
        }
        let url = args["url"].as_str().unwrap_or_default().trim();
        if url.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "url is empty"));
        }
        if let Err(e) = check_url(url) {
            return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, e.to_string()));
        }
        let mut headers = Vec::new();
        if let Some(obj) = args.get("headers").and_then(Value::as_object) {
            for (name, value) in obj {
                let Some(value) = value.as_str() else {
                    return Ok(ToolResult::err(
                        ToolErrorCode::InvalidArgs,
                        format!("header {name:?} must be a string"),
                    ));
                };
                headers.push((name.clone(), value.to_string()));
            }
        }
        let body = if method == "GET" || method == "HEAD" {
            None
        } else {
            args.get("body").and_then(Value::as_str)
        };
        let timeout = args
            .get("timeoutSec")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1.0, MAX_TIMEOUT_SECS);
        let max_bytes = args
            .get("maxBytes")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_MAX_BYTES as f64)
            .clamp(1.0, MAX_MAX_BYTES as f64) as u64;
        let resp = match http_request(
            &method,
            url,
            &headers,
            body,
            max_bytes,
            Duration::from_secs_f64(timeout),
        ) {
            Ok(resp) => resp,
            // a redirect hop onto a denied host keeps the same verdict as
            // the initial URL check
            Err(e) if e.contains("ssrf") => {
                return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, e))
            }
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::ExecutionError, e)),
        };
        let (encoding, body) = if is_pass_through(&resp.content_type) {
            ("text", String::from_utf8_lossy(&resp.body).into_owned())
        } else {
            (
                "base64",
                base64::engine::general_purpose::STANDARD.encode(&resp.body),
            )
        };
        let headers: serde_json::Map<String, Value> = resp
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), json!(value)))
            .collect();
        Ok(ToolResult::ok(json!({
            "status": resp.status,
            "finalUrl": resp.final_url,
            "contentType": resp.content_type,
            "headers": headers,
            "body": body,
            "encoding": encoding,
            "truncated": resp.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::mcp::mock_server::{respond, Handler, MockServer};
    use std::sync::Arc;

    fn mock(handler: Handler) -> MockServer {
        MockServer::start(handler)
    }

    #[test]
    fn http_request_get_returns_status_headers_and_text_body() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, req| match req.path.as_str() {
            "/api" => respond(stream, 200, "application/json", "{\"ok\":true}", &[("X-Api-Version", "2")]),
            _ => respond(stream, 404, "application/json", "{\"error\":\"nope\"}", &[]),
        }));
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "get", "url": format!("{}/api", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["status"], 200);
        assert_eq!(data["encoding"], "text");
        assert_eq!(data["body"], "{\"ok\":true}");
        assert_eq!(data["headers"]["x-api-version"], "2");
        assert_eq!(data["finalUrl"], format!("{}/api", server.url_base));
        assert_eq!(data["truncated"], false);

        // non-2xx comes back as data, not as a tool error
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": format!("{}/missing", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["status"], 404);
    }

    #[test]
    fn http_request_post_sends_method_body_and_headers_to_the_server() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 201, "text/plain", "created", &[])
        }));
        let r = HttpRequestTool
            .execute(
                &json!({
                    "method": "POST",
                    "url": format!("{}/create", server.url_base),
                    "headers": { "Content-Type": "application/json", "X-Token": "secret" },
                    "body": "{\"q\":\"hi\"}"
                }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["status"], 201);
        assert_eq!(data["body"], "created");
        let log = server.request_log();
        assert_eq!(log[0].method, "POST");
        assert_eq!(log[0].path, "/create");
        assert_eq!(log[0].body, "{\"q\":\"hi\"}");
        assert_eq!(log[0].headers["content-type"], "application/json");
        assert_eq!(log[0].headers["x-token"], "secret");
    }

    #[test]
    fn http_request_head_returns_status_without_body() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "text/plain", "", &[("X-Head", "yes")])
        }));
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "HEAD", "url": format!("{}/ping", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["status"], 200);
        assert_eq!(data["body"], "");
        assert_eq!(data["headers"]["x-head"], "yes");
        assert_eq!(server.request_log()[0].method, "HEAD");
    }

    #[test]
    fn http_request_follows_redirects_and_denies_a_redirect_to_a_ssrf_host() {
        let server = mock(Arc::new(
            |stream: &mut std::net::TcpStream, req| match req.path.as_str() {
                "/start" => respond(stream, 302, "text/plain", "", &[("Location", "/final")]),
                "/post-redirect" => respond(stream, 303, "text/plain", "", &[("Location", "/final")]),
                "/to-metadata" => respond(
                    stream,
                    302,
                    "text/plain",
                    "",
                    &[("Location", "http://169.254.169.254/late")],
                ),
                _ => respond(stream, 200, "text/plain", "landed", &[]),
            },
        ));
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": format!("{}/start", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["finalUrl"], format!("{}/final", server.url_base));
        assert_eq!(data["body"], "landed");

        // 303 re-issues the follow-up request as a bodyless GET (the log is
        // shared with the GET subtest above: entries 2 and 3 are this call)
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "POST", "url": format!("{}/post-redirect", server.url_base), "body": "x" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let log = server.request_log();
        assert_eq!(log[2].method, "POST");
        assert_eq!(log[3].method, "GET");
        assert_eq!(log[3].path, "/final");

        // a redirect hop to a denied host is refused, never fetched
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": format!("{}/to-metadata", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        assert!(r.error.as_ref().unwrap().message.contains("ssrf"));
    }

    #[test]
    fn http_request_refuses_ssrf_hosts_and_invalid_methods() {
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": "http://169.254.169.254/latest/meta-data/" }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert_eq!(r.error.as_ref().unwrap().code, "permission_denied");
        assert!(r.error.unwrap().message.contains("ssrf"));

        for method in ["TRACE", "OPTIONS", "CONNECT", "fetch", ""] {
            let r = HttpRequestTool
                .execute(&json!({ "method": method, "url": "http://127.0.0.1:9/" }), &ToolCtx::default())
                .unwrap();
            assert_eq!(r.error.as_ref().unwrap().code, "invalid_args", "method {method:?}");
        }
        let r = HttpRequestTool
            .execute(&json!({ "method": "GET", "url": "  " }), &ToolCtx::default())
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
    }

    #[test]
    fn http_request_caps_body_at_max_bytes_and_envelope_at_max_result_len() {
        let body = "y".repeat(40_000);
        let server = mock(Arc::new(move |stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "text/plain", &body, &[])
        }));
        // explicit maxBytes truncates the body and flags it
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": format!("{}/big", server.url_base), "maxBytes": 100 }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["body"].as_str().unwrap().len(), 100);
        assert_eq!(data["truncated"], true);

        // the default cap downloads everything but the envelope must stay
        // inside max_result_len: an oversized result becomes limit_exceeded
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": format!("{}/big", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "limit_exceeded");
    }

    #[test]
    fn http_request_returns_binary_bodies_as_base64() {
        let server = mock(Arc::new(|stream: &mut std::net::TcpStream, _req| {
            respond(stream, 200, "image/png", "not really png", &[])
        }));
        let r = HttpRequestTool
            .execute(
                &json!({ "method": "GET", "url": format!("{}/img", server.url_base) }),
                &ToolCtx::default(),
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["encoding"], "base64");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data["body"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"not really png");
    }

    #[test]
    fn http_request_policy_confirms_only_mutating_methods() {
        assert_eq!(HttpRequestTool.class(), ToolClass::Mutating);
        assert_eq!(HttpRequestTool.policy(), PermissionPolicy::CheckConditional);
        for method in ["GET", "get", "HEAD", "head"] {
            assert!(
                HttpRequestTool
                    .danger_reason(&json!({ "method": method, "url": "http://x.example/a" }), &ToolCtx::default())
                    .is_none(),
                "{method} must skip confirmation"
            );
        }
        for method in ["POST", "put", "PATCH", "delete"] {
            let reason = HttpRequestTool
                .danger_reason(&json!({ "method": method, "url": "http://x.example/a" }), &ToolCtx::default())
                .unwrap();
            assert_eq!(reason, format!("HTTP {} http://x.example/a", method.to_uppercase()));
        }
    }

    #[test]
    fn http_request_schema_accepts_full_call_and_rejects_bad_headers() {
        let schema = HttpRequestTool.parameters();
        assert!(validate_args(&schema, &json!({ "method": "GET", "url": "https://api.example.com/v1" })).is_ok());
        assert!(validate_args(
            &schema,
            &json!({
                "method": "POST", "url": "https://api.example.com/v1",
                "headers": { "Authorization": "Bearer t" }, "body": "{}",
                "timeoutSec": 5, "maxBytes": 1000
            })
        )
        .is_ok());
        assert!(validate_args(&schema, &json!({ "method": "GET" })).is_err());
        assert!(validate_args(
            &schema,
            &json!({ "method": "GET", "url": "https://x", "headers": { "a": 5 } })
        )
        .is_err());
        assert!(validate_args(
            &schema,
            &json!({ "method": "GET", "url": "https://x", "extra": 1 })
        )
        .is_err());
    }

    #[test]
    #[ignore = "live network: GET example.com via http_request"]
    fn live_http_request_get_example_com() {
        let r = HttpRequestTool
            .execute(&json!({ "method": "GET", "url": "https://example.com/" }), &ToolCtx::default())
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["status"], 200);
        assert!(data["body"].as_str().unwrap().contains("example"));
    }
}
