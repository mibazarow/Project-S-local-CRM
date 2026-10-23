use crate::error::{AppError, Result};
use crate::llm::config::ChatParams;
use crate::llm::models::{parse_models, ConnectionTest, ModelInfo};
use crate::llm::reasoning::{ensure_reasoning_on_assistants, is_reasoning_passback_error};
use crate::llm::streaming::{parse_non_stream, run_sse};
use crate::llm::types::{ChatEvent, ChatMessage, ChatOutcome, ToolSpec};
use serde_json::{json, Value};
use std::time::Duration;

pub struct LlmClient {
    base_url: String,
    api_key: String,
    agent: ureq::Agent,
    // Streaming agent: its per-read socket timeout IS the inactivity watchdog
    // (SO_RCVTIMEO resets on every read), so a slow-but-live stream survives
    // while a silent provider aborts at the threshold.
    stream_agent: ureq::Agent,
    stream_timeout: Duration,
}

// Provider-silence abort threshold (SPEC 6.0.1, Q23).
pub const DEFAULT_STREAM_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(180);

impl LlmClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self::with_stream_timeout(base_url, api_key, DEFAULT_STREAM_INACTIVITY_TIMEOUT)
    }

    // Test seam: override the streaming inactivity threshold so a test can
    // abort a silent provider without waiting the real 180 s.
    pub fn with_stream_timeout(base_url: &str, api_key: &str, stream_timeout: Duration) -> Self {
        LlmClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.trim().to_string(),
            // Non-streaming: timeout_read is per socket read, so a slow first
            // token or a long stream is fine as long as data keeps arriving.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(180))
                .build(),
            // Streaming: the per-read socket timeout is the inactivity watchdog
            // (resets on every read), so slow-but-live generation survives and
            // a provider that stops sending bytes aborts at the threshold.
            stream_agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(stream_timeout)
                .build(),
            stream_timeout,
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
        self.call_with_retry_raw(make)
            .map_err(|e| self.map_err(e))
    }

    // Same 429/5xx/transport retry, but hands the raw ureq error back so the
    // chat paths can inspect a 400 body before it is mapped into an AppError.
    fn call_with_retry_raw(
        &self,
        make: impl Fn() -> std::result::Result<ureq::Response, Box<ureq::Error>>,
    ) -> std::result::Result<ureq::Response, ureq::Error> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match make() {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt < 2 && Self::is_retryable(&e) => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(*e),
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
        self.chat_stream_with_cancel(messages, tools, params, &mut on_event, None)
    }

    pub fn chat_stream_with_cancel(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
        on_event: &mut dyn FnMut(ChatEvent),
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<ChatOutcome> {
        let body = build_body(messages, tools, params, true);
        let resp = self.post_chat_with_reasoning_retry(&body, |b| self.post_chat_stream_raw(b))?;
        run_sse(resp.into_reader(), self.stream_timeout, on_event, cancel)
    }

    // Non-streaming variant, same request builder (used by tests and fallbacks).
    pub fn chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolSpec]>,
        params: &ChatParams,
    ) -> Result<ChatOutcome> {
        let body = build_body(messages, tools, params, false);
        let resp = self.post_chat_with_reasoning_retry(&body, |b| self.post_chat_raw(b))?;
        let value: Value = resp
            .into_json()
            .map_err(|e| AppError::Json(self.masked(format!("chat response is not valid JSON: {e}"))))?;
        parse_non_stream(&value)
    }

    fn post_chat_raw(&self, body: &Value) -> std::result::Result<ureq::Response, ureq::Error> {
        self.call_with_retry_raw(|| {
            self.auth(
                self.agent
                    .post(&format!("{}/chat/completions", self.base_url)),
            )
            .send_json(body)
            .map_err(Box::new)
        })
    }

    // Streaming POST on the watchdog agent. Single attempt (no 429/5xx retry):
    // a silent provider must abort at the inactivity bound and surface for the
    // user's «Повторить», not silently double the wait.
    fn post_chat_stream_raw(
        &self,
        body: &Value,
    ) -> std::result::Result<ureq::Response, ureq::Error> {
        self.auth(
            self.stream_agent
                .post(&format!("{}/chat/completions", self.base_url)),
        )
        .send_json(body)
    }

    // Sends a chat request (streaming or not) and, on the thinking-mode
    // "reasoning_content must be passed back" 400, retries exactly once with a
    // corrected body where every assistant turn carries the field. Provider-
    // agnostic: the config heuristic may have missed a gateway, and requests
    // without tools were never patched proactively. A second failure surfaces
    // as the usual HTTP error.
    fn post_chat_with_reasoning_retry(
        &self,
        body: &Value,
        post: impl Fn(&Value) -> std::result::Result<ureq::Response, ureq::Error>,
    ) -> Result<ureq::Response> {
        match post(body) {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(status, resp)) => {
                let err_body = resp.into_string().unwrap_or_default();
                if !is_reasoning_passback_error(status, &err_body) {
                    return Err(self.status_err(status, err_body));
                }
                let mut fixed = body.clone();
                ensure_reasoning_on_assistants(fixed["messages"].as_array_mut());
                post(&fixed).map_err(|e| self.map_err(e))
            }
            Err(e) => Err(self.map_err(e)),
        }
    }
}

pub(crate) fn build_body(
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
    params: &ChatParams,
    stream: bool,
) -> Value {
    let tools = tools.filter(|t| !t.is_empty());
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
    if let Some(tools) = tools {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| json!({ "type": "function", "function": t }))
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    // DeepSeek thinking mode + tools: every assistant turn must carry
    // `reasoning_content`, or the API rejects the follow-up with HTTP 400. The
    // serializer already emits the real trace when present; here we only fill
    // the turns that have none (synthetic summaries, no-CoT rounds) with an
    // empty string. Stored rows are never rewritten — this is wire-level only.
    if params.requires_reasoning_passback && tools.is_some() {
        ensure_reasoning_on_assistants(body["messages"].as_array_mut());
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ContentPart, ToolCall};
    use serde_json::json;
    use std::time::Duration;

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
            requires_reasoning_passback: false,
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
            requires_reasoning_passback: false,
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
            requires_reasoning_passback: false,
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
    fn stream_inactivity_timeout_is_injectable() {
        // The test seam lets a caller shrink the watchdog without a real 180 s.
        let client = LlmClient::with_stream_timeout("https://example.test/v1", "k", Duration::from_millis(5));
        assert_eq!(client.stream_timeout, Duration::from_millis(5));
        // The default constructor keeps the shipped 180 s threshold.
        assert_eq!(
            LlmClient::new("https://example.test/v1", "k").stream_timeout,
            DEFAULT_STREAM_INACTIVITY_TIMEOUT
        );
        assert_eq!(DEFAULT_STREAM_INACTIVITY_TIMEOUT, Duration::from_secs(180));
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
