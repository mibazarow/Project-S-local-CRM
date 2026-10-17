// Chat API below is consumed by the agent loop slice (M3b), not by commands yet.
#![allow(dead_code)]

pub mod types;

pub use types::*;

use crate::config::{read_config, ConfigKind, LlmConfig};
use crate::error::{AppError, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ChatParams {
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl From<&LlmConfig> for ChatParams {
    fn from(cfg: &LlmConfig) -> Self {
        ChatParams {
            model: cfg.model.clone(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTest {
    pub ok: bool,
    pub message: String,
    pub model_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    pub id: String,
}

pub struct LlmClient {
    base_url: String,
    api_key: String,
    agent: ureq::Agent,
}

// Reads llm.json fresh on every call so hand-edits and settings writes apply
// without restarting the app.
pub fn client_from_config(wd: &Path) -> Result<LlmClient> {
    let cfg: LlmConfig = serde_json::from_value(read_config(wd, ConfigKind::Llm)?)?;
    if cfg.base_url.trim().is_empty() {
        return Err(AppError::Invalid("llm baseUrl is empty".into()));
    }
    Ok(LlmClient::new(&cfg.base_url, &cfg.api_key))
}

impl LlmClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        LlmClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.trim().to_string(),
            // timeout_read is per socket read, so a slow first token or a long
            // stream is fine as long as data keeps arriving.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(120))
                .build(),
        }
    }

    // The key must never reach logs or UI error strings.
    fn masked(&self, msg: String) -> String {
        if self.api_key.is_empty() {
            msg
        } else {
            msg.replace(&self.api_key, "***")
        }
    }

    fn auth(&self, req: ureq::Request) -> ureq::Request {
        if self.api_key.is_empty() {
            req
        } else {
            req.set("Authorization", &format!("Bearer {}", self.api_key))
        }
    }

    fn is_retryable(e: &ureq::Error) -> bool {
        match e {
            ureq::Error::Status(code, _) => *code == 429 || *code >= 500,
            ureq::Error::Transport(_) => true,
        }
    }

    fn status_err(&self, code: u16, body: String) -> AppError {
        let brief: String = body.chars().take(300).collect();
        AppError::Http(self.masked(format!("HTTP {code}: {brief}")))
    }

    fn map_err(&self, e: ureq::Error) -> AppError {
        match e {
            ureq::Error::Status(code, resp) => {
                self.status_err(code, resp.into_string().unwrap_or_default())
            }
            ureq::Error::Transport(t) => AppError::Http(self.masked(format!("connection error: {t}"))),
        }
    }

    // One retry on 429/5xx/transport errors; never after the body started
    // streaming. ureq::Error embeds a full Response, so box it to keep the
    // closure return type small.
    fn call_with_retry(
        &self,
        make: impl Fn() -> std::result::Result<ureq::Response, Box<ureq::Error>>,
    ) -> Result<ureq::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match make() {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt < 2 && Self::is_retryable(&e) => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(self.map_err(*e)),
            }
        }
    }

    pub fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self.call_with_retry(|| {
            self.auth(
                self.agent
                    .get(&format!("{}/models", self.base_url))
                    .timeout(Duration::from_secs(30)),
            )
            .call()
            .map_err(Box::new)
        })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| AppError::Json(self.masked(format!("models response is not valid JSON: {e}"))))?;
        parse_models(&body)
    }

    pub fn test_connection(&self) -> ConnectionTest {
        match self.list_models() {
            Ok(models) => ConnectionTest {
                ok: true,
                message: format!("Подключение в порядке: получено {} моделей", models.len()),
                model_count: models.len(),
            },
            Err(e) => ConnectionTest {
                ok: false,
                message: e.to_string(),
                model_count: 0,
            },
        }
    }

    pub fn chat_stream(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        mut on_event: impl FnMut(ChatEvent),
    ) -> Result<ChatOutcome> {
        let body = build_body(messages, tools, params, true);
        let resp = self.post_chat(&body)?;
        run_sse(resp.into_reader(), &mut on_event)
    }

    // Non-streaming variant, same request builder (used by tests and fallbacks).
    pub fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
    ) -> Result<ChatOutcome> {
        let body = build_body(messages, tools, params, false);
        let resp = self.post_chat(&body)?;
        let value: Value = resp
            .into_json()
            .map_err(|e| AppError::Json(self.masked(format!("chat response is not valid JSON: {e}"))))?;
        parse_non_stream(&value)
    }

    fn post_chat(&self, body: &Value) -> Result<ureq::Response> {
        self.call_with_retry(|| {
            self.auth(
                self.agent
                    .post(&format!("{}/chat/completions", self.base_url)),
            )
            .send_json(body)
            .map_err(Box::new)
        })
    }
}

fn parse_models(body: &Value) -> Result<Vec<ModelInfo>> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Json("models response has no \"data\" array".into()))?;
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .map(|id| ModelInfo { id: id.to_string() })
        .collect())
}

pub(crate) fn build_body(
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
    params: &ChatParams,
    stream: bool,
) -> Value {
    let mut body = json!({
        "model": params.model,
        "messages": messages,
        "stream": stream,
    });
    if let Some(t) = params.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = params.max_tokens {
        body["max_tokens"] = json!(m);
    }
    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| json!({ "type": "function", "function": t }))
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    body
}

#[derive(Default)]
struct StreamAcc {
    content: String,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    // index-keyed fragments: (id, name, arguments) accumulated across chunks.
    frags: BTreeMap<usize, (String, String, String)>,
}

impl StreamAcc {
    fn apply(&mut self, chunk: &Value, on_event: &mut impl FnMut(ChatEvent)) {
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            if let Ok(usage) = serde_json::from_value(u.clone()) {
                self.usage = Some(usage);
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            if !c.is_empty() {
                self.content.push_str(c);
                on_event(ChatEvent::Delta(c.to_string()));
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tcs {
                let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let frag = self.frags.entry(index).or_default();
                let id = tc.get("id").and_then(Value::as_str);
                if let Some(id) = id {
                    frag.0.push_str(id);
                }
                let func = tc.get("function");
                let name = func.and_then(|f| f.get("name")).and_then(Value::as_str);
                if let Some(name) = name {
                    frag.1.push_str(name);
                }
                let arguments = func
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                frag.2.push_str(arguments);
                on_event(ChatEvent::ToolCallDelta {
                    index,
                    id: id.map(String::from),
                    name: name.map(String::from),
                    arguments_delta: arguments.to_string(),
                });
            }
        }
    }

    fn finish(self, on_event: &mut impl FnMut(ChatEvent)) -> ChatOutcome {
        let tool_calls: Vec<ToolCall> = self
            .frags
            .into_iter()
            .map(|(i, (id, name, arguments))| {
                let id = if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                };
                ToolCall::new(id, name, arguments)
            })
            .collect();
        if !tool_calls.is_empty() {
            on_event(ChatEvent::ToolCallsFinal(tool_calls.clone()));
        }
        on_event(ChatEvent::Usage(self.usage.clone()));
        on_event(ChatEvent::Done);
        ChatOutcome {
            content: (!self.content.is_empty()).then_some(self.content),
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

fn run_sse(reader: impl std::io::Read, on_event: &mut impl FnMut(ChatEvent)) -> Result<ChatOutcome> {
    let mut acc = StreamAcc::default();
    let mut buf = std::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        if buf.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(payload)?;
        acc.apply(&chunk, on_event);
    }
    Ok(acc.finish(on_event))
}

fn parse_non_stream(value: &Value) -> Result<ChatOutcome> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| AppError::Json("chat response has no choices".into()))?;
    let message = &choice["message"];
    let content = message.get("content").and_then(Value::as_str).map(String::from);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let func = tc.get("function")?;
                    Some(ToolCall::new(
                        tc.get("id").and_then(Value::as_str)?.to_string(),
                        func.get("name").and_then(Value::as_str)?.to_string(),
                        func.get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ChatOutcome {
        content,
        tool_calls,
        finish_reason: choice.get("finish_reason").and_then(Value::as_str).map(String::from),
        usage: value
            .get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn sse(lines: &[&str]) -> (Vec<ChatEvent>, ChatOutcome) {
        let text: String = lines.iter().map(|l| format!("{l}\n\n")).collect();
        let mut events = Vec::new();
        let outcome = run_sse(Cursor::new(text), &mut |e| events.push(e)).unwrap();
        (events, outcome)
    }

    #[test]
    fn sse_stream_accumulates_text_and_split_tool_call() {
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\": "}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}"#,
            "data: [DONE]",
        ]);

        assert_eq!(outcome.content.as_deref(), Some("Hello"));
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(outcome.usage.clone().unwrap().total_tokens, 12);
        assert_eq!(outcome.tool_calls.len(), 1);
        let tc = &outcome.tool_calls[0];
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "read_file");
        assert_eq!(tc.arguments, json!({ "path": "a.txt" }));
        assert!(tc.arguments_error.is_none());

        let deltas = events
            .iter()
            .filter(|e| matches!(e, ChatEvent::Delta(_)))
            .count();
        assert_eq!(deltas, 2);
        assert!(events.iter().any(|e| matches!(e, ChatEvent::ToolCallDelta { index: 0, .. })));
        assert!(matches!(events.last(), Some(ChatEvent::Done)));
    }

    #[test]
    fn sse_invalid_tool_arguments_keep_raw_and_mark_error() {
        let (_, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{oops"}}]}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "data: [DONE]",
        ]);
        let tc = &outcome.tool_calls[0];
        assert_eq!(tc.raw_arguments, "{oops");
        assert!(tc.arguments_error.is_some());
        assert_eq!(tc.arguments, Value::Null);
    }

    #[test]
    fn request_body_matches_openai_wire_shape() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{\"a\":1}".into(),
            )]),
            ChatMessage::tool_result("c1", "ok"),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let params = ChatParams {
            model: "m".into(),
            temperature: Some(0.5),
            max_tokens: Some(64),
        };
        let body = build_body(&messages, Some(&tools), &params, true);
        assert_eq!(
            body,
            json!({
                "model": "m",
                "messages": [
                    { "role": "system", "content": "sys" },
                    { "role": "user", "content": "hi" },
                    {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "c1",
                            "type": "function",
                            "function": { "name": "f", "arguments": "{\"a\":1}" }
                        }]
                    },
                    { "role": "tool", "content": "ok", "tool_call_id": "c1" }
                ],
                "stream": true,
                "temperature": 0.5,
                "max_tokens": 64,
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "f",
                        "description": "d",
                        "parameters": { "type": "object" }
                    }
                }],
                "tool_choice": "auto"
            })
        );
    }

    #[test]
    fn non_stream_response_parses_to_same_outcome() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hi",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "f", "arguments": "{\"a\":1}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 }
        });
        let outcome = parse_non_stream(&value).unwrap();
        assert_eq!(outcome.content.as_deref(), Some("hi"));
        assert_eq!(outcome.tool_calls[0].arguments, json!({ "a": 1 }));
        assert_eq!(outcome.usage.unwrap().total_tokens, 3);
    }

    #[test]
    fn parse_models_extracts_ids_and_rejects_bad_shape() {
        let models = parse_models(&json!({ "data": [{ "id": "gpt-x" }, { "id": "gpt-y" }, { "name": 7 }] }))
            .unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["gpt-x", "gpt-y"]
        );
        assert!(matches!(
            parse_models(&json!({ "error": { "message": "nope" } })),
            Err(AppError::Json(_))
        ));
    }

    #[test]
    fn non_200_maps_to_typed_error_without_leaking_key() {
        let client = LlmClient::new("https://example.test/v1", "sk-secret-123");
        let err = client.status_err(401, "invalid api key sk-secret-123".into());
        let msg = err.to_string();
        assert!(matches!(err, AppError::Http(_)));
        assert!(msg.contains("HTTP 401"));
        assert!(msg.contains("***"));
        assert!(!msg.contains("sk-secret-123"));
    }

    #[test]
    #[ignore = "hits a real endpoint; set PROJECT_S_LLM_BASE_URL / PROJECT_S_LLM_API_KEY"]
    fn live_list_models() {
        let base = std::env::var("PROJECT_S_LLM_BASE_URL").unwrap_or_default();
        let key = std::env::var("PROJECT_S_LLM_API_KEY").unwrap_or_default();
        let models = LlmClient::new(&base, &key).list_models().unwrap();
        assert!(!models.is_empty());
    }
}
