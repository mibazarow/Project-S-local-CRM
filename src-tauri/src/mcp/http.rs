// Streamable HTTP MCP transport (SPEC 6.2): every JSON-RPC request is POSTed
// to the configured endpoint with Accept: application/json, text/event-stream
// plus the configured auth headers. The server answers either with a plain
// JSON body or with an SSE body that is read until the matching id arrives.
// Mcp-Session-Id from any response is stored and echoed on later requests.
// Timeouts are enforced by ureq (connect + per-read), so send() blocks only
// as long as the server keeps the response alive and recv() just drains what
// send() already fetched.
use super::jsonrpc::McpTransport;
use super::sse::{map_ureq_err, read_event};
use crate::error::{AppError, Result};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::io::BufReader;
use std::time::{Duration, Instant};

const ACCEPT: &str = "application/json, text/event-stream";

pub struct HttpTransport {
    url: String,
    headers: BTreeMap<String, String>,
    agent: ureq::Agent,
    session_id: Option<String>,
    pending: VecDeque<Value>,
    call_timeout: Duration,
}

impl HttpTransport {
    pub fn new(
        url: &str,
        headers: &BTreeMap<String, String>,
        call_timeout: Duration,
    ) -> Result<HttpTransport> {
        Ok(HttpTransport {
            url: url.to_string(),
            headers: headers.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(call_timeout)
                .build(),
            session_id: None,
            pending: VecDeque::new(),
            call_timeout,
        })
    }
}

impl McpTransport for HttpTransport {
    fn send(&mut self, msg: &Value) -> Result<()> {
        let mut req = self.agent.post(&self.url).set("Accept", ACCEPT);
        for (name, value) in &self.headers {
            req = req.set(name, value);
        }
        if let Some(session) = &self.session_id {
            req = req.set("Mcp-Session-Id", session);
        }
        let resp = req.send_json(msg).map_err(map_ureq_err)?;
        if let Some(session) = resp.header("Mcp-Session-Id") {
            self.session_id = Some(session.to_string());
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            // Notifications are acknowledged with 202 Accepted, no body.
            return Ok(());
        };
        if resp.content_type().starts_with("text/event-stream") {
            let mut reader = BufReader::new(resp.into_reader());
            let deadline = Instant::now() + self.call_timeout;
            loop {
                if Instant::now() > deadline {
                    return Err(AppError::Http(
                        "sse response did not contain the matching id in time".into(),
                    ));
                }
                let Some(event) = read_event(&mut reader)
                    .map_err(|e| AppError::Http(format!("sse read error: {e}")))?
                else {
                    return Err(AppError::Http(
                        "sse response ended without the matching id".into(),
                    ));
                };
                if event.data.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(event.data.trim())?;
                let matched = value.get("id").and_then(Value::as_u64) == Some(id);
                self.pending.push_back(value);
                if matched {
                    return Ok(());
                }
            }
        }
        let body = resp
            .into_string()
            .map_err(|e| AppError::Http(format!("response read error: {e}")))?;
        let value: Value = serde_json::from_str(&body)?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(AppError::Http(format!(
                "http response id does not match request {id}"
            )));
        }
        self.pending.push_back(value);
        Ok(())
    }

    fn recv(&mut self, _timeout: Duration) -> Result<Value> {
        self.pending
            .pop_front()
            .ok_or_else(|| AppError::Http("http transport has no pending response".into()))
    }

    fn close(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::mock_server::{
        mcp_answer, respond, write_sse_headers, CapturedRequest, Handler, MockServer,
    };
    use crate::mcp::McpClient;
    use serde_json::{json, Value};
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::Arc;

    fn auth_headers() -> BTreeMap<String, String> {
        BTreeMap::from([("Authorization".to_string(), "Bearer s3cret-token".to_string())])
    }

    // Plain-JSON streamable HTTP server that also hands out a session id.
    fn json_handler(session: &'static str) -> Handler {
        Arc::new(move |stream: &mut TcpStream, req: &CapturedRequest| {
            let body: Value = serde_json::from_str(&req.body).unwrap();
            if body["method"].as_str() == Some("notifications/initialized") {
                return respond(stream, 202, "application/json", "", &[("Mcp-Session-Id", session)]);
            }
            let answer = mcp_answer(body["method"].as_str().unwrap_or(""), &body["id"]);
            respond(
                stream,
                200,
                "application/json",
                &answer.to_string(),
                &[("Mcp-Session-Id", session)],
            )
        })
    }

    #[test]
    fn streamable_http_round_trips_and_echoes_session_and_auth_headers() {
        let server = MockServer::start(json_handler("sess-42"));
        let transport =
            HttpTransport::new(&format!("{}/mcp", server.url_base), &auth_headers(), Duration::from_secs(120)).unwrap();
        let mut client =
            McpClient::connect_with(Box::new(transport), Duration::from_secs(120)).unwrap();
        let tools = client.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let call = client.call_tool("echo", &json!({ "text": "hi" })).unwrap();
        assert_eq!(call.text, "echo: hi");
        client.close();
        let log = server.request_log();
        assert_eq!(log.len(), 4); // initialize, initialized, tools/list, tools/call
        assert_eq!(log[0].path, "/mcp");
        assert_eq!(log[0].headers["accept"], ACCEPT);
        assert_eq!(log[0].headers["authorization"], "Bearer s3cret-token");
        assert!(log[0].headers.get("content-type").unwrap().starts_with("application/json"));
        assert!(!log[0].headers.contains_key("mcp-session-id"));
        for req in &log[1..] {
            assert_eq!(req.headers["mcp-session-id"], "sess-42");
        }
    }

    #[test]
    fn streamable_http_parses_sse_bodies_and_skips_other_ids() {
        let handler: Handler = Arc::new(|stream: &mut TcpStream, req: &CapturedRequest| {
            let body: Value = serde_json::from_str(&req.body).unwrap();
            let method = body["method"].as_str().unwrap_or("");
            if method == "notifications/initialized" {
                return respond(stream, 202, "application/json", "", &[]);
            }
            let answer = mcp_answer(method, &body["id"]);
            write_sse_headers(stream)?;
            stream.write_all(b": keepalive\r\n\r\n")?;
            stream.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"stale\":true}}\r\n\r\n")?;
            stream.write_all(format!("event: message\ndata: {answer}\r\n\r\n").as_bytes())
        });
        let server = MockServer::start(handler);
        let transport =
            HttpTransport::new(&format!("{}/mcp", server.url_base), &BTreeMap::new(), Duration::from_secs(120)).unwrap();
        let mut client =
            McpClient::connect_with(Box::new(transport), Duration::from_secs(120)).unwrap();
        let tools = client.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        let call = client.call_tool("echo", &json!({ "text": "hi" })).unwrap();
        assert_eq!(call.text, "echo: hi");
    }

    #[test]
    fn streamable_http_maps_error_status_to_app_error() {
        let handler: Handler =
            Arc::new(|stream: &mut TcpStream, _req| respond(stream, 401, "text/plain", "unauthorized", &[]));
        let server = MockServer::start(handler);
        let transport =
            HttpTransport::new(&format!("{}/mcp", server.url_base), &auth_headers(), Duration::from_secs(120)).unwrap();
        let err = match McpClient::connect_with(Box::new(transport), Duration::from_secs(120)) {
            Ok(_) => panic!("connect must fail on 401"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("HTTP 401"));
        assert!(err.to_string().contains("unauthorized"));
    }
}
