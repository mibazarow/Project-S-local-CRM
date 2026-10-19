pub mod types;

pub use types::*;

use crate::config::{read_config, write_config, ConfigKind, LlmConfig};
use crate::error::{AppError, Result};
use base64::Engine as _;
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
    // Thinking-mode effort ("low" | "medium" | "high"); None omits
    // `reasoning_effort` from the request entirely (thinking off).
    pub reasoning_effort: Option<String>,
}

impl From<&LlmConfig> for ChatParams {
    fn from(cfg: &LlmConfig) -> Self {
        ChatParams {
            model: cfg.model.clone(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
            reasoning_effort: cfg.reasoning_effort().map(str::to_owned),
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

// Cap on image bytes sent to the model as a data URL (base64 inflates ~33%).
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

// Reads an image file into a base64 data URL for OpenAI-compatible image
// content parts. Rejects non-image extensions and files over MAX_IMAGE_BYTES.
pub fn image_data_url(path: &Path) -> Result<String> {
    crate::fs_ops::binary_data_url(path, MAX_IMAGE_BYTES, &path.display().to_string())
}

// Canonical minimal 1x1 transparent PNG: the probe payload and test fixture.
pub(crate) const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
    0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // IHDR 1x1 RGBA
    0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05,
    0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, // IDAT
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // IEND
];

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VisionStatus {
    // None = unknown (never probed, or the probe was inconclusive).
    pub supported: Option<bool>,
    // True when llm.json `vision` manually overrides detection.
    pub overridden: bool,
    pub reason: Option<String>,
}

// Probes whether the configured model accepts image content parts: one
// minimal chat request with a tiny 1x1 PNG part and max_tokens 1. A manual
// `vision` override in llm.json short-circuits the probe; a definitive result
// is cached in `visionDetected`.
pub fn detect_vision(wd: &Path) -> Result<VisionStatus> {
    let cfg: LlmConfig = serde_json::from_value(read_config(wd, ConfigKind::Llm)?)?;
    if let Some(v) = cfg.vision {
        return Ok(VisionStatus {
            supported: Some(v),
            overridden: true,
            reason: None,
        });
    }
    if cfg.base_url.trim().is_empty() {
        return Err(AppError::Invalid("llm baseUrl is empty".into()));
    }
    let client = LlmClient::new(&cfg.base_url, &cfg.api_key);
    let params = ChatParams {
        model: cfg.model.clone(),
        temperature: None,
        max_tokens: Some(1),
        reasoning_effort: None,
    };
    let probe_url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(TINY_PNG)
    );
    let messages = vec![ChatMessage::user_parts(vec![
        ContentPart::text("Reply with just OK"),
        ContentPart::image_url(probe_url),
    ])];
    match client.chat(&messages, None, &params) {
        Ok(_) => {
            cache_vision_detected(wd, true)?;
            Ok(VisionStatus {
                supported: Some(true),
                overridden: false,
                reason: None,
            })
        }
        Err(e) => {
            let msg = e.to_string();
            if is_capability_error(&msg) {
                cache_vision_detected(wd, false)?;
                let reason: String = msg.chars().take(200).collect();
                Ok(VisionStatus {
                    supported: Some(false),
                    overridden: false,
                    reason: Some(reason),
                })
            } else {
                // Network/auth/unknown failure: never claim the model lacks
                // vision on inconclusive evidence.
                Ok(VisionStatus {
                    supported: None,
                    overridden: false,
                    reason: Some(msg),
                })
            }
        }
    }
}

// Cached/overridden vision status without any network call.
pub fn get_vision_status(wd: &Path) -> Result<VisionStatus> {
    let cfg: LlmConfig = serde_json::from_value(read_config(wd, ConfigKind::Llm)?)?;
    if let Some(v) = cfg.vision {
        return Ok(VisionStatus {
            supported: Some(v),
            overridden: true,
            reason: None,
        });
    }
    Ok(VisionStatus {
        supported: cfg.vision_detected,
        overridden: false,
        reason: None,
    })
}

fn cache_vision_detected(wd: &Path, detected: bool) -> Result<()> {
    write_config(wd, ConfigKind::Llm, json!({ "visionDetected": detected }))
}

// 400/415/422 on a request that is otherwise well-formed: the provider
// rejected the image content part itself, so the model lacks vision.
// 401/403 (auth), 404 (wrong endpoint/model), 429/5xx (transient) and
// transport errors say nothing about vision capability.
fn is_capability_error(msg: &str) -> bool {
    let code = msg
        .find("HTTP ")
        .and_then(|i| {
            msg[i + 5..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u16>()
                .ok()
        })
        .unwrap_or(0);
    matches!(code, 400 | 415 | 422)
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
    if let Some(effort) = &params.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
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
    reasoning: String,
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
        // Thinking deltas: DeepSeek-style `reasoning_content`, some gateways
        // stream the same trace as `reasoning`. Both accumulate and stream;
        // only one key per delta is read so an aliasing provider cannot
        // double-count the same fragment.
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str);
        if let Some(r) = reasoning {
            if !r.is_empty() {
                self.reasoning.push_str(r);
                on_event(ChatEvent::ReasoningDelta(r.to_string()));
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
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
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
    let reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .map(String::from);
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
        reasoning,
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
    use crate::onboarding::temp_test_dir;
    use serde_json::json;
    use std::fs;
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
    fn sse_stream_flushes_one_delta_per_content_chunk_in_order() {
        // Progressive display depends on the backend emitting one Delta per SSE
        // data event (no buffering to the full response): N content chunks must
        // yield N ordered, non-empty deltas whose concatenation is the answer.
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"content":"При"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"вет"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"!"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        let streamed: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, ["При", "вет", "!"]);
        assert_eq!(outcome.content.as_deref(), Some("Привет!"));
    }

    #[test]
    fn sse_stream_one_chunk_delivers_the_whole_answer_in_a_single_delta() {
        // A provider that buffers the whole answer into one SSE chunk still
        // yields exactly one delta carrying the full text (the frontend then
        // reveals it with a typewriter fallback).
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"content":"Весь ответ одним куском"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        let deltas: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Delta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Весь ответ одним куском"]);
        assert_eq!(outcome.content.as_deref(), Some("Весь ответ одним куском"));
    }

    #[test]
    fn sse_stream_accumulates_reasoning_from_both_delta_keys() {
        let (events, outcome) = sse(&[
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"Thin"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"king"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"reasoning":" deeper"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(outcome.reasoning.as_deref(), Some("Thinking deeper"));
        assert_eq!(outcome.content.as_deref(), Some("Hi"));
        let streamed: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ReasoningDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, ["Thin", "king", " deeper"]);
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
            ChatMessage::assistant("earlier answer").with_reasoning(Some("earlier thought".into())),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{\"a\":1}".into(),
            )])
            .with_reasoning(Some("planning".into())),
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
            reasoning_effort: Some("medium".into()),
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
                        "content": "earlier answer",
                        "reasoning_content": "earlier thought"
                    },
                    {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": "planning",
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
                "reasoning_effort": "medium",
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
    fn thinking_off_omits_reasoning_effort_from_the_body() {
        let params = ChatParams {
            model: "m".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        };
        let body = build_body(&[ChatMessage::user("hi")], None, &params, false);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_body_serializes_multimodal_user_content_parts() {
        let messages = vec![ChatMessage::user_parts(vec![
            ContentPart::text("что на картинке?"),
            ContentPart::image_url("data:image/png;base64,AAA="),
        ])];
        let params = ChatParams {
            model: "m".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        };
        let body = build_body(&messages, None, &params, false);
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                { "type": "text", "text": "что на картинке?" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA=" } }
            ])
        );
    }

    #[test]
    fn image_data_url_encodes_png_and_rejects_non_image_and_oversize() {
        let dir = temp_test_dir("llm-image-url");
        fs::write(dir.join("pic.png"), TINY_PNG).unwrap();
        let url = image_data_url(&dir.join("pic.png")).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        let b64 = url.split_once(',').unwrap().1;
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(b64).unwrap(),
            TINY_PNG
        );

        fs::write(dir.join("note.txt"), b"not an image").unwrap();
        assert!(matches!(
            image_data_url(&dir.join("note.txt")),
            Err(AppError::Invalid(_))
        ));

        fs::write(dir.join("big.png"), vec![0u8; MAX_IMAGE_BYTES as usize + 1]).unwrap();
        assert!(matches!(image_data_url(&dir.join("big.png")), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detect_vision_manual_override_returns_without_a_probe() {
        // baseUrl points at a closed local port: reaching the network at all
        // would fail the call, so passing proves the override short-circuits.
        let dir = temp_test_dir("llm-vision-override");
        write_config(
            &dir,
            ConfigKind::Llm,
            json!({ "baseUrl": "http://127.0.0.1:1/v1", "model": "m", "vision": true }),
        )
        .unwrap();
        let st = detect_vision(&dir).unwrap();
        assert_eq!(st.supported, Some(true));
        assert!(st.overridden);
        assert!(st.reason.is_none());

        write_config(&dir, ConfigKind::Llm, json!({ "vision": false })).unwrap();
        let st = detect_vision(&dir).unwrap();
        assert_eq!((st.supported, st.overridden), (Some(false), true));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn get_vision_status_reports_override_and_cache_without_probing() {
        let dir = temp_test_dir("llm-vision-status");
        write_config(&dir, ConfigKind::Llm, json!({ "baseUrl": "http://127.0.0.1:1/v1", "model": "m" }))
            .unwrap();
        let st = get_vision_status(&dir).unwrap();
        assert_eq!((st.supported, st.overridden), (None, false));

        write_config(&dir, ConfigKind::Llm, json!({ "visionDetected": false })).unwrap();
        let st = get_vision_status(&dir).unwrap();
        assert_eq!((st.supported, st.overridden), (Some(false), false));

        write_config(&dir, ConfigKind::Llm, json!({ "vision": true })).unwrap();
        let st = get_vision_status(&dir).unwrap();
        assert_eq!((st.supported, st.overridden), (Some(true), true));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn capability_error_classification_is_conservative() {
        assert!(is_capability_error("http error: HTTP 400: model does not support image inputs"));
        assert!(is_capability_error("http error: HTTP 415: unsupported media type"));
        assert!(!is_capability_error("http error: HTTP 401: invalid api key"));
        assert!(!is_capability_error("http error: HTTP 404: not found"));
        assert!(!is_capability_error("http error: HTTP 429: rate limited"));
        assert!(!is_capability_error("http error: HTTP 503: upstream down"));
        assert!(!is_capability_error("http error: connection error: dns failure"));
    }

    #[test]
    fn non_stream_response_parses_to_same_outcome() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hi",
                    "reasoning_content": "because",
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
        assert_eq!(outcome.reasoning.as_deref(), Some("because"));
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
