// Newline-delimited JSON-RPC 2.0 framing plus an id-correlated request
// channel over a transport-agnostic seam (SPEC 6.2). A later HTTP/SSE
// transport implements the same trait over POST bodies instead of pipes.
use crate::error::{AppError, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub fn encode_line(msg: &Value) -> Result<String> {
    Ok(serde_json::to_string(msg)? + "\n")
}

pub fn decode_line(line: &str) -> Result<Value> {
    Ok(serde_json::from_str(line)?)
}

pub fn request_msg(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn notification_msg(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

pub trait McpTransport: Send {
    fn send(&mut self, msg: &Value) -> Result<()>;
    fn recv(&mut self, timeout: Duration) -> Result<Value>;
    fn close(&mut self);
}

pub struct RpcChannel {
    transport: Box<dyn McpTransport>,
    next_id: u64,
}

impl RpcChannel {
    pub fn new(transport: Box<dyn McpTransport>) -> Self {
        RpcChannel { transport, next_id: 1 }
    }

    // Sends a request and returns its result, skipping notifications and
    // responses for other ids until the matching id arrives.
    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.transport.send(&request_msg(id, method, params))?;
        loop {
            let msg = self.transport.recv(timeout)?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                return Err(AppError::Http(format!("json-rpc error {code}: {message}")));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.transport.send(&notification_msg(method, params))
    }

    pub fn close(&mut self) {
        self.transport.close();
    }
}

#[cfg(test)]
pub(crate) struct MockTransport {
    incoming: std::collections::VecDeque<Value>,
    sent: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}

#[cfg(test)]
impl MockTransport {
    pub(crate) fn new(responses: Vec<Value>) -> Self {
        MockTransport {
            incoming: responses.into(),
            sent: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn sent_log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<Value>>> {
        std::sync::Arc::clone(&self.sent)
    }
}

#[cfg(test)]
impl McpTransport for MockTransport {
    fn send(&mut self, msg: &Value) -> Result<()> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(())
    }
    fn recv(&mut self, _timeout: Duration) -> Result<Value> {
        self.incoming
            .pop_front()
            .ok_or_else(|| AppError::Io("mock transport exhausted".into()))
    }
    fn close(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn encode_produces_a_single_line_and_decode_round_trips() {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "nested": { "deep": [1, 2] } } });
        let line = encode_line(&msg).unwrap();
        assert!(line.ends_with('\n'));
        assert!(!line[..line.len() - 1].contains('\n'));
        assert_eq!(decode_line(line.trim_end()).unwrap(), msg);
    }

    #[test]
    fn request_correlates_by_id_skipping_notifications_and_other_ids() {
        let mock = MockTransport::new(vec![
            json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 7, "result": { "stale": true } }),
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } }),
        ]);
        let sent = mock.sent_log();
        let mut channel = RpcChannel::new(Box::new(mock));
        let result = channel.request("tools/list", json!({}), Duration::from_secs(1)).unwrap();
        assert_eq!(result, json!({ "ok": true }));
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0]["id"], json!(1));
        assert_eq!(sent[0]["method"], json!("tools/list"));
    }

    #[test]
    fn request_maps_json_rpc_error_with_code_and_message() {
        let mock = MockTransport::new(vec![json!({
            "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "method not found" }
        })]);
        let mut channel = RpcChannel::new(Box::new(mock));
        let err = channel.request("bogus", json!({}), Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("-32601"));
        assert!(err.to_string().contains("method not found"));
    }

    #[test]
    fn notify_sends_a_message_without_id() {
        let mock = MockTransport::new(vec![]);
        let sent = mock.sent_log();
        let mut channel = RpcChannel::new(Box::new(mock));
        channel.notify("notifications/initialized", json!({})).unwrap();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].get("id").is_none());
    }
}
