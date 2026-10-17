// Test-only localhost HTTP mock (no external network): binds 127.0.0.1:0,
// parses each request and hands the live socket to a handler closure, so a
// test can also stream SSE bodies. Every request is captured for assertions.
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) struct CapturedRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) body: String,
}

pub(crate) type Handler =
    Arc<dyn Fn(&mut TcpStream, &CapturedRequest) -> std::io::Result<()> + Send + Sync>;

pub(crate) struct MockServer {
    pub(crate) url_base: String,
    stop: Arc<AtomicBool>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl MockServer {
    pub(crate) fn start(handler: Handler) -> MockServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accept_thread = {
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests);
                            let handler = Arc::clone(&handler);
                            std::thread::spawn(move || {
                                let _ = handle_conn(stream, requests, handler);
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        MockServer {
            url_base: format!("http://127.0.0.1:{port}"),
            stop,
            accept_thread: Some(accept_thread),
            requests,
        }
    }

    pub(crate) fn request_log(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_conn(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    handler: Handler,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let req = {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut headers = BTreeMap::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                headers.insert(name.trim().to_lowercase(), value.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = String::new();
        if len > 0 {
            reader.take(len as u64).read_to_string(&mut body)?;
        }
        CapturedRequest {
            method,
            path,
            headers,
            body,
        }
    };
    requests.lock().unwrap().push(req.clone());
    handler(&mut stream, &req)?;
    stream.flush()
}

pub(crate) fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )?;
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(body.as_bytes())
}

// Headers for an open-ended SSE response: no Content-Length, the body runs
// until the connection closes.
pub(crate) fn write_sse_headers(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
}

// Canned MCP answers shared by the transport tests.
pub(crate) fn mcp_answer(method: &str, id: &Value) -> Value {
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "protocolVersion": crate::mcp::PROTOCOL_VERSION,
                "capabilities": {},
                "serverInfo": { "name": "mock", "version": "0" },
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "tools": [{
                "name": "echo",
                "description": "Echoes the given text",
                "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
            }] }
        }),
        "tools/call" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": { "content": [{ "type": "text", "text": "echo: hi" }] }
        }),
        other => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("unknown method {other}") }
        }),
    }
}
