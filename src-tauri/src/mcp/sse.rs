// Legacy HTTP+SSE MCP transport (SPEC 6.2): GET the configured SSE endpoint,
// learn the POST endpoint from the first `endpoint` event, keep the stream
// open on a reader thread, POST every request to the endpoint and correlate
// responses arriving on the stream by id. The reader wakes on a short socket
// timeout so close()/drop can stop it without hanging; a stream that closes
// early surfaces as a clear transport error.
use super::jsonrpc::{decode_line, McpTransport};
use crate::error::{AppError, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CONNECT_WAIT: Duration = Duration::from_secs(15);
// Per-read socket timeout on the open stream: bounds how long close() can
// take to stop the reader and how fast a dead connection is noticed.
const STREAM_POLL: Duration = Duration::from_secs(2);
const POST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

pub(crate) fn map_ureq_err(e: ureq::Error) -> AppError {
    match e {
        ureq::Error::Status(code, resp) => {
            let brief: String = resp.into_string().unwrap_or_default().chars().take(300).collect();
            AppError::Http(format!("HTTP {code}: {brief}"))
        }
        ureq::Error::Transport(t) => AppError::Http(format!("connection error: {t}")),
    }
}

// Reads one blank-line-terminated SSE event (comments and unknown fields are
// skipped, multi-line data joined with \n); Ok(None) means the stream ended.
// A blank line always ends an event, even a comment-only keepalive one, so
// callers stay responsive to shutdown on busy streams.
pub(crate) fn read_event<R: Read>(reader: &mut BufReader<R>) -> std::io::Result<Option<SseEvent>> {
    let mut event: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Ok(Some(SseEvent {
                event,
                data: data_lines.join("\n"),
            }));
        }
        if line.starts_with(':') {
            continue;
        }
        let (name, value) = match line.split_once(':') {
            Some((name, value)) => (name.trim(), value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match name {
            "event" => event = Some(value.to_string()),
            "data" => data_lines.push(value.to_string()),
            _ => {}
        }
    }
}

// The endpoint event may carry an absolute URL, a root-relative path or a
// path relative to the SSE URL; resolve it against the SSE URL origin.
fn resolve_endpoint(base: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    let after_scheme = base.find("://").map_or(0, |i| i + 3);
    let authority_end =
        after_scheme + base[after_scheme..].find(['/', '?', '#']).unwrap_or(base.len() - after_scheme);
    let origin = &base[..authority_end];
    if let Some(path) = endpoint.strip_prefix('/') {
        format!("{origin}/{path}")
    } else {
        let path = &base[authority_end..];
        let dir = path.rfind('/').map_or("", |i| &path[..i]);
        format!("{origin}{dir}/{endpoint}")
    }
}

pub struct SseTransport {
    post_url: String,
    headers: BTreeMap<String, String>,
    agent: ureq::Agent,
    stop: Arc<AtomicBool>,
    messages: Receiver<Result<Value>>,
    reader: Option<JoinHandle<()>>,
}

impl SseTransport {
    pub fn connect(url: &str, headers: &BTreeMap<String, String>) -> Result<SseTransport> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(STREAM_POLL)
            .build();
        let mut req = agent.get(url).set("Accept", "text/event-stream");
        for (name, value) in headers {
            req = req.set(name, value);
        }
        let resp = req.call().map_err(map_ureq_err)?;
        if !resp.content_type().starts_with("text/event-stream") {
            return Err(AppError::Http(format!(
                "expected text/event-stream from {url}, got {}",
                resp.content_type()
            )));
        }
        let mut stream = BufReader::new(resp.into_reader());
        let deadline = Instant::now() + CONNECT_WAIT;
        let post_url = loop {
            match read_event(&mut stream) {
                Ok(None) => {
                    return Err(AppError::Http(
                        "sse stream closed before the endpoint event".into(),
                    ))
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                    if Instant::now() > deadline {
                        return Err(AppError::Http(
                            "timed out waiting for the sse endpoint event".into(),
                        ));
                    }
                }
                Err(e) => return Err(AppError::Http(format!("sse read error: {e}"))),
                Ok(Some(event)) => {
                    if event.event.as_deref() == Some("endpoint") && !event.data.trim().is_empty() {
                        break resolve_endpoint(url, event.data.trim());
                    }
                }
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, messages) = mpsc::channel();
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            loop {
                // Checked every iteration: a busy stream (keepalive pings)
                // never times out, so stop must be polled here as well.
                if reader_stop.load(Ordering::SeqCst) {
                    break;
                }
                match read_event(&mut stream) {
                    Ok(None) => {
                        let _ = tx.send(Err(AppError::Io("mcp sse stream closed by server".into())));
                        break;
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                        if reader_stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(AppError::Http(format!("sse read error: {e}"))));
                        break;
                    }
                    Ok(Some(event)) => {
                        let data = event.data.trim();
                        if data.is_empty() {
                            continue;
                        }
                        match decode_line(data) {
                            // Keepalive comments and non-JSON payloads are skipped.
                            Ok(msg) => {
                                if tx.send(Ok(msg)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                }
            }
        });
        Ok(SseTransport {
            post_url,
            headers: headers.clone(),
            agent,
            stop,
            messages,
            reader: Some(reader),
        })
    }
}

impl McpTransport for SseTransport {
    fn send(&mut self, msg: &Value) -> Result<()> {
        let mut req = self
            .agent
            .post(&self.post_url)
            .timeout(POST_TIMEOUT);
        for (name, value) in &self.headers {
            req = req.set(name, value);
        }
        // The legacy transport acknowledges POSTs with 202; the real answer
        // arrives on the SSE stream and is correlated by RpcChannel.
        req.send_json(msg).map_err(map_ureq_err)?;
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Value> {
        match self.messages.recv_timeout(timeout) {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Err(AppError::Http(format!(
                "mcp sse server did not answer within {}s",
                timeout.as_secs()
            ))),
            Err(RecvTimeoutError::Disconnected) => {
                Err(AppError::Io("mcp sse reader thread stopped".into()))
            }
        }
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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
    use std::collections::VecDeque;
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};

    fn auth_headers() -> BTreeMap<String, String> {
        BTreeMap::from([("Authorization".to_string(), "Bearer s3cret-token".to_string())])
    }

    #[test]
    fn endpoint_urls_resolve_absolute_root_relative_and_relative() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9/sse", "https://other.example/msg"),
            "https://other.example/msg"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9/base/sse", "/messages?sid=42"),
            "http://127.0.0.1:9/messages?sid=42"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9/base/sse", "messages"),
            "http://127.0.0.1:9/base/messages"
        );
    }

    #[test]
    fn legacy_sse_endpoint_event_and_correlated_stream_responses() {
        let queue: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let handler: Handler = {
            let queue = Arc::clone(&queue);
            Arc::new(move |stream: &mut TcpStream, req: &CapturedRequest| {
                match req.method.as_str() {
                    "GET" => {
                        write_sse_headers(stream)?;
                        stream.write_all(b"event: endpoint\ndata: /messages?sid=42\r\n\r\n")?;
                        stream.flush()?;
                        loop {
                            let drained: Vec<String> = queue.lock().unwrap().drain(..).collect();
                            for msg in drained {
                                stream.write_all(format!("data: {msg}\r\n\r\n").as_bytes())?;
                            }
                            stream.write_all(b": ping\r\n\r\n")?;
                            stream.flush()?;
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                    }
                    "POST" => {
                        let body: Value = serde_json::from_str(&req.body).unwrap();
                        let method = body["method"].as_str().unwrap_or("");
                        if !method.starts_with("notifications/") {
                            let answer = mcp_answer(method, &body["id"]).to_string();
                            queue.lock().unwrap().push_back(answer);
                        }
                        respond(stream, 202, "application/json", "", &[])
                    }
                    _ => respond(stream, 405, "application/json", "", &[]),
                }
            })
        };
        let server = MockServer::start(handler);
        let transport =
            SseTransport::connect(&format!("{}/sse", server.url_base), &auth_headers()).unwrap();
        let mut client =
            McpClient::connect_with(Box::new(transport), Duration::from_secs(120)).unwrap();
        let tools = client.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let call = client.call_tool("echo", &json!({ "text": "hi" })).unwrap();
        assert_eq!(call.text, "echo: hi");
        client.close();
        let log = server.request_log();
        assert_eq!(log[0].method, "GET");
        assert_eq!(log[0].path, "/sse");
        assert_eq!(log[0].headers["accept"], "text/event-stream");
        assert_eq!(log[0].headers["authorization"], "Bearer s3cret-token");
        for req in &log[1..] {
            assert_eq!(req.method, "POST");
            assert_eq!(req.path, "/messages?sid=42");
            assert_eq!(req.headers["authorization"], "Bearer s3cret-token");
        }
    }

    #[test]
    fn legacy_sse_stream_closed_by_server_surfaces_clear_error() {
        let handler: Handler = Arc::new(|stream: &mut TcpStream, req: &CapturedRequest| {
            match req.method.as_str() {
                "GET" => {
                    write_sse_headers(stream)?;
                    stream.write_all(b"event: endpoint\ndata: /messages\r\n\r\n")
                }
                "POST" => respond(stream, 202, "application/json", "", &[]),
                _ => respond(stream, 405, "application/json", "", &[]),
            }
        });
        let server = MockServer::start(handler);
        let transport =
            SseTransport::connect(&format!("{}/sse", server.url_base), &BTreeMap::new()).unwrap();
        // The endpoint event arrives, then the server hangs up: the reader
        // thread reports the closed stream and initialize fails cleanly.
        let err = match McpClient::connect_with(Box::new(transport), Duration::from_secs(120)) {
            Ok(_) => panic!("connect must fail on a closed stream"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("sse stream closed by server"));
    }
}
