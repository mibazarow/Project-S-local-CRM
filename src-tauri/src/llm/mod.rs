pub mod types;

pub use types::*;

use crate::config::{
    migrate_llm_providers, new_provider_id, read_config, write_config, ConfigKind, LlmConfig,
    LlmProvider,
};
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
    // Provider requires every assistant turn to carry `reasoning_content`
    // (empty string when the turn produced none) whenever the request carries
    // `tools` — DeepSeek thinking mode rejects a follow-up that omits it with
    // HTTP 400 "reasoning_content must be passed back". False keeps the legacy
    // behavior (send the trace only when it is non-empty) for other providers.
    pub requires_reasoning_passback: bool,
}

// DeepSeek thinking mode is the only provider we gate on today: it requires the
// reasoning pass-back whenever a request carries tools. Detected from the
// effective endpoint/model name; thinking off means there is no trace to pass
// back, so the requirement does not apply.
fn reasoning_passback_required(cfg: &LlmConfig) -> bool {
    let deepseek = cfg
        .effective_base_url()
        .to_ascii_lowercase()
        .contains("deepseek")
        || cfg.model.to_ascii_lowercase().contains("deepseek");
    deepseek && cfg.reasoning_effort().is_some()
}

impl From<&LlmConfig> for ChatParams {
    fn from(cfg: &LlmConfig) -> Self {
        ChatParams {
            model: cfg.model.clone(),
            temperature: cfg.temperature,
            max_tokens: cfg.max_tokens,
            reasoning_effort: cfg.reasoning_effort().map(str::to_owned),
            requires_reasoning_passback: reasoning_passback_required(cfg),
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
    // Streaming agent: its per-read socket timeout IS the inactivity watchdog
    // (SO_RCVTIMEO resets on every read), so a slow-but-live stream survives
    // while a silent provider aborts at the threshold.
    stream_agent: ureq::Agent,
    stream_timeout: Duration,
}

// Provider-silence abort threshold (SPEC 6.0.1, Q23).
pub const DEFAULT_STREAM_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(40);

// Reads llm.json fresh on every call so hand-edits and settings writes apply
// without restarting the app. Resolves the ACTIVE provider (Q-24), falling back
// to the legacy top-level fields when no provider is active.
pub fn client_from_config(wd: &Path) -> Result<LlmClient> {
    let cfg: LlmConfig = serde_json::from_value(read_config(wd, ConfigKind::Llm)?)?;
    let base_url = cfg.effective_base_url();
    if base_url.trim().is_empty() {
        return Err(AppError::Invalid("llm baseUrl is empty".into()));
    }
    let api_key = cfg.effective_api_key();
    if api_key.trim().is_empty() && !is_local_host(base_url) {
        return Err(AppError::Invalid(
            "API-ключ не задан у активного провайдера".into(),
        ));
    }
    Ok(LlmClient::new(base_url, api_key))
}

impl LlmClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self::with_stream_timeout(base_url, api_key, DEFAULT_STREAM_INACTIVITY_TIMEOUT)
    }

    // Test seam: override the streaming inactivity threshold so a test can
    // abort a silent provider without waiting the real 40 s.
    pub fn with_stream_timeout(base_url: &str, api_key: &str, stream_timeout: Duration) -> Self {
        LlmClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.trim().to_string(),
            // Non-streaming: timeout_read is per socket read, so a slow first
            // token or a long stream is fine as long as data keeps arriving.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(120))
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
        let body = build_body(messages, tools, params, true);
        let resp = self.post_chat_with_reasoning_retry(&body, |b| self.post_chat_stream_raw(b))?;
        run_sse(resp.into_reader(), self.stream_timeout, &mut on_event)
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
    if cfg.effective_base_url().trim().is_empty() {
        return Err(AppError::Invalid("llm baseUrl is empty".into()));
    }
    let client = LlmClient::new(cfg.effective_base_url(), cfg.effective_api_key());
    let params = ChatParams {
        model: cfg.model.clone(),
        temperature: None,
        max_tokens: Some(1),
        reasoning_effort: None,
        requires_reasoning_passback: false,
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

// ---------- Provider profiles (Q-24) ----------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmProviderDto {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub active: bool,
}

fn to_dtos(cfg: &LlmConfig) -> Vec<LlmProviderDto> {
    let active = cfg.active_provider.as_deref();
    cfg.providers
        .iter()
        .map(|p| LlmProviderDto {
            id: p.id.clone(),
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone(),
            active: Some(p.id.as_str()) == active,
        })
        .collect()
}

// True when the endpoint host is local (Ollama-style servers run without a key).
fn is_local_host(base_url: &str) -> bool {
    let after = base_url
        .trim()
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url.trim());
    let host = after.split(['/', ':', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn validate_provider(name: &str, base_url: &str, api_key: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(AppError::Invalid("Имя провайдера не может быть пустым".into()));
    }
    let url = base_url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(AppError::Invalid(
            "Endpoint должен начинаться с http:// или https://".into(),
        ));
    }
    if api_key.trim().is_empty() && !is_local_host(url) {
        return Err(AppError::Invalid(
            "API-ключ пуст — разрешено только для localhost/127.0.0.1".into(),
        ));
    }
    Ok(())
}

// Reads llm.json (seeding providers on first run), applies `f`, mirrors the
// active provider into the legacy top-level fields, and writes back only the
// provider keys so every other key (model, vision, hand-edits) survives.
fn mutate_providers(
    wd: &Path,
    f: impl FnOnce(&mut LlmConfig) -> Result<()>,
) -> Result<Vec<LlmProviderDto>> {
    migrate_llm_providers(wd)?;
    let mut cfg: LlmConfig = serde_json::from_value(read_config(wd, ConfigKind::Llm)?)?;
    f(&mut cfg)?;
    let mirror = cfg
        .active()
        .map(|a| (a.base_url.clone(), a.api_key.clone()));
    if let Some((base_url, api_key)) = mirror {
        cfg.base_url = base_url;
        cfg.api_key = api_key;
    }
    let dtos = to_dtos(&cfg);
    write_config(
        wd,
        ConfigKind::Llm,
        json!({
            "providers": cfg.providers,
            "activeProvider": cfg.active_provider,
            "baseUrl": cfg.base_url,
            "apiKey": cfg.api_key,
        }),
    )?;
    Ok(dtos)
}

pub fn providers_list(wd: &Path) -> Result<Vec<LlmProviderDto>> {
    migrate_llm_providers(wd)?;
    let cfg: LlmConfig = serde_json::from_value(read_config(wd, ConfigKind::Llm)?)?;
    Ok(to_dtos(&cfg))
}

pub fn provider_add(wd: &Path, name: &str, base_url: &str, api_key: &str) -> Result<Vec<LlmProviderDto>> {
    let (name, base_url, api_key) = (name.trim(), base_url.trim(), api_key.trim());
    validate_provider(name, base_url, api_key)?;
    mutate_providers(wd, |cfg| {
        if cfg.providers.iter().any(|p| p.name.trim() == name) {
            return Err(AppError::Invalid(format!("Провайдер «{name}» уже есть")));
        }
        let id = new_provider_id();
        cfg.providers.push(LlmProvider {
            id: id.clone(),
            name: name.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        });
        if cfg.active_provider.is_none() {
            cfg.active_provider = Some(id);
        }
        Ok(())
    })
}

pub fn provider_update(
    wd: &Path,
    id: &str,
    name: &str,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<LlmProviderDto>> {
    let (name, base_url, api_key) = (name.trim(), base_url.trim(), api_key.trim());
    validate_provider(name, base_url, api_key)?;
    mutate_providers(wd, |cfg| {
        let idx = cfg
            .providers
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| AppError::Invalid("Провайдер не найден".into()))?;
        if cfg
            .providers
            .iter()
            .enumerate()
            .any(|(i, p)| i != idx && p.name.trim() == name)
        {
            return Err(AppError::Invalid(format!("Провайдер «{name}» уже есть")));
        }
        let p = &mut cfg.providers[idx];
        p.name = name.into();
        p.base_url = base_url.into();
        p.api_key = api_key.into();
        Ok(())
    })
}

pub fn provider_delete(wd: &Path, id: &str) -> Result<Vec<LlmProviderDto>> {
    mutate_providers(wd, |cfg| {
        if cfg.providers.len() <= 1 {
            return Err(AppError::Invalid("Нельзя удалить последний провайдер".into()));
        }
        if cfg.active_provider.as_deref() == Some(id) {
            return Err(AppError::Invalid(
                "Нельзя удалить активный провайдер — сначала выберите другой".into(),
            ));
        }
        cfg.providers.retain(|p| p.id != id);
        Ok(())
    })
}

pub fn provider_activate(wd: &Path, id: &str) -> Result<Vec<LlmProviderDto>> {
    mutate_providers(wd, |cfg| {
        if !cfg.providers.iter().any(|p| p.id == id) {
            return Err(AppError::Invalid("Провайдер не найден".into()));
        }
        cfg.active_provider = Some(id.into());
        Ok(())
    })
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

// Adds `"reasoning_content": ""` to every assistant message object that lacks
// the key, leaving turns that already carry a trace untouched.
fn ensure_reasoning_on_assistants(messages: Option<&mut Vec<Value>>) {
    if let Some(messages) = messages {
        for m in messages {
            if m.get("role").and_then(Value::as_str) == Some("assistant")
                && m.get("reasoning_content").is_none()
            {
                m["reasoning_content"] = json!("");
            }
        }
    }
}

// The thinking-mode pass-back rejection: HTTP 400 whose body names
// `reasoning_content` anywhere (case-insensitive; JSON wrapping, quoting and
// backticks around the field name are irrelevant to the substring match).
// Deliberately not keyed on provider or model names: any gateway enforcing the
// rule answers with this exact class of error.
fn is_reasoning_passback_error(status: u16, body: &str) -> bool {
    status == 400 && body.to_ascii_lowercase().contains("reasoning_content")
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

fn run_sse(
    reader: impl std::io::Read,
    inactivity: Duration,
    on_event: &mut impl FnMut(ChatEvent),
) -> Result<ChatOutcome> {
    let mut acc = StreamAcc::default();
    let mut buf = std::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            // The stream agent's per-read socket timeout surfaces here as a
            // TimedOut io error once `inactivity` passes with zero bytes. Map
            // it to a distinct, user-readable error; the reader (and its
            // socket) is dropped on return, closing the connection.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(AppError::LlmTimeout(format!(
                    "Провайдер не отвечает {} секунд — проверьте баланс, API-ключ и статус провайдера.",
                    inactivity.as_secs()
                )));
            }
            Err(e) => return Err(e.into()),
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
        let outcome = run_sse(
            Cursor::new(text),
            DEFAULT_STREAM_INACTIVITY_TIMEOUT,
            &mut |e| events.push(e),
        )
        .unwrap();
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
    fn reasoning_passback_gate_is_deepseek_and_thinking_only() {
        let deepseek_on = crate::config::LlmConfig {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            thinking_level: "medium".into(),
            ..Default::default()
        };
        assert!(reasoning_passback_required(&deepseek_on));
        assert!(ChatParams::from(&deepseek_on).requires_reasoning_passback);

        // DeepSeek endpoint but thinking off: there is no trace to pass back.
        let deepseek_off = crate::config::LlmConfig {
            thinking_level: "off".into(),
            ..deepseek_on.clone()
        };
        assert!(!reasoning_passback_required(&deepseek_off));

        // A DeepSeek model name behind a non-deepseek base URL still trips it.
        let model_named = crate::config::LlmConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "DeepSeek-R1".into(),
            ..Default::default()
        };
        assert!(reasoning_passback_required(&model_named));

        // OpenAI with thinking on keeps the legacy behavior (no forced pass-back).
        let openai = crate::config::LlmConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            thinking_level: "high".into(),
            ..Default::default()
        };
        assert!(!reasoning_passback_required(&openai));
        assert!(!ChatParams::from(&openai).requires_reasoning_passback);
    }

    #[test]
    fn build_body_fills_missing_reasoning_on_assistant_turns_for_deepseek() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant("plain answer"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{}".into(),
            )]),
            ChatMessage::assistant("traced").with_reasoning(Some("real thought".into())),
            ChatMessage::tool_result("c1", "ok"),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let params = ChatParams {
            model: "deepseek-chat".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: true,
        };
        let body = build_body(&messages, Some(&tools), &params, false);
        let msgs = body["messages"].as_array().unwrap();
        // Non-assistant roles are never given the key.
        assert!(msgs[0].get("reasoning_content").is_none());
        assert!(msgs[1].get("reasoning_content").is_none());
        assert!(msgs[5].get("reasoning_content").is_none());
        // Assistant turns without a trace (plain and tool-call) get an empty one.
        assert_eq!(msgs[2]["reasoning_content"], "");
        assert!(msgs[3].get("tool_calls").is_some());
        assert_eq!(msgs[3]["reasoning_content"], "");
        // An existing non-empty trace is preserved, not overwritten.
        assert_eq!(msgs[4]["reasoning_content"], "real thought");
    }

    #[test]
    fn build_body_adds_no_reasoning_field_for_other_providers() {
        let messages = vec![
            ChatMessage::assistant("plain"),
            ChatMessage::assistant_tool_calls(vec![ToolCall::new(
                "c1".into(),
                "f".into(),
                "{}".into(),
            )]),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];
        let params = ChatParams {
            model: "gpt-4o-mini".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let body = build_body(&messages, Some(&tools), &params, false);
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[0].get("reasoning_content").is_none());
        assert!(msgs[1].get("reasoning_content").is_none());
    }

    #[test]
    fn build_body_passback_is_noop_without_tools() {
        let messages = vec![ChatMessage::assistant("plain")];
        let params = ChatParams {
            model: "deepseek-chat".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: true,
        };
        let body = build_body(&messages, None, &params, false);
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_mock_rejects_body_missing_reasoning_passback() {
        use crate::mcp::mock_server::{respond, CapturedRequest, Handler, MockServer};

        // DeepSeek-like gate: HTTP 400 unless every assistant turn carries
        // `reasoning_content`; 200 with a valid completion otherwise.
        let handler: Handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let missing = body["messages"]
                    .as_array()
                    .map(|msgs| {
                        msgs.iter().any(|m| {
                            m.get("role").and_then(Value::as_str) == Some("assistant")
                                && m.get("reasoning_content").is_none()
                        })
                    })
                    .unwrap_or(false);
                if missing {
                    respond(
                        stream,
                        400,
                        "application/json",
                        r#"{"error":{"message":"reasoning_content must be passed back"}}"#,
                        &[],
                    )
                } else {
                    respond(
                        stream,
                        200,
                        "application/json",
                        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
                        &[],
                    )
                }
            },
        );
        let server = MockServer::start(handler);
        let client = LlmClient::new(&server.url_base, "k");

        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("earlier"),
            ChatMessage::user("again"),
        ];
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];

        // Gate on (DeepSeek thinking): the empty trace is filled in proactively,
        // so the provider accepts the follow-up on the first attempt.
        let fixed = ChatParams {
            model: "deepseek-chat".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: true,
        };
        let out = client.chat(&messages, Some(&tools), &fixed).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        assert_eq!(server.request_log().len(), 1);

        // The wire body actually carried the empty trace on the assistant turn.
        let sent: Value =
            serde_json::from_str(&server.request_log().last().unwrap().body).unwrap();
        assert_eq!(sent["messages"][1]["reasoning_content"], "");

        // Gate off: the proactive fill is skipped, the mock 400s, and the
        // provider-agnostic reasoning retry repairs the follow-up on the
        // second attempt.
        let legacy = ChatParams {
            requires_reasoning_passback: false,
            ..fixed.clone()
        };
        let out = client.chat(&messages, Some(&tools), &legacy).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 3);
        let first: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert!(first["messages"][1].get("reasoning_content").is_none());
        let retry: Value = serde_json::from_str(&sent[2].body).unwrap();
        assert_eq!(retry["messages"][1]["reasoning_content"], "");
    }

    #[test]
    fn reasoning_passback_error_detection_is_provider_agnostic() {
        // The exact production shape (backticks, request id, JSON wrapping).
        assert!(is_reasoning_passback_error(
            400,
            r#"{"error":{"message":"The `reasoning_content` in the thinking mode must be passed back to the API. (request_id: 94a2e983-5ada-4c47-9499-0ff7821cd23b)","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#
        ));
        assert!(is_reasoning_passback_error(400, "REASONING_CONTENT MUST BE PASSED BACK"));
        assert!(is_reasoning_passback_error(400, "  reasoning_content  "));
        // Other 400s and other statuses saying the word are not this error.
        assert!(!is_reasoning_passback_error(400, "model does not support image inputs"));
        assert!(!is_reasoning_passback_error(401, "reasoning_content"));
        assert!(!is_reasoning_passback_error(500, "reasoning_content"));
    }

    // A gateway under a non-DeepSeek name that always enforces the pass-back:
    // 400 while any assistant turn omits the field, 200 otherwise.
    fn passback_gate_handler() -> crate::mcp::mock_server::Handler {
        use crate::mcp::mock_server::{respond, CapturedRequest};
        std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let missing = body["messages"]
                    .as_array()
                    .map(|msgs| {
                        msgs.iter().any(|m| {
                            m.get("role").and_then(Value::as_str) == Some("assistant")
                                && m.get("reasoning_content").is_none()
                        })
                    })
                    .unwrap_or(false);
                if missing {
                    respond(
                        stream,
                        400,
                        "application/json",
                        r#"{"error":{"message":"The `reasoning_content` in the thinking mode must be passed back to the API.","type":"invalid_request_error"}}"#,
                        &[],
                    )
                } else {
                    respond(
                        stream,
                        200,
                        "application/json",
                        r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
                        &[],
                    )
                }
            },
        )
    }

    #[test]
    fn reasoning_400_retries_once_with_and_without_tools() {
        use crate::mcp::mock_server::MockServer;

        let server = MockServer::start(passback_gate_handler());
        let client = LlmClient::new(&server.url_base, "k");
        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("earlier").with_reasoning(Some("real trace".into())),
            ChatMessage::assistant("plain"),
            ChatMessage::user("again"),
        ];
        // Heuristic gate off: neither the endpoint nor the model says deepseek.
        let params = ChatParams {
            model: "reasoner-v2".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let tools = [ToolSpec {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({ "type": "object" }),
        }];

        // With tools: exactly one retry; the retry preserves the real trace
        // and fills the untraced assistant turn with an empty one.
        let out = client.chat(&messages, Some(&tools), &params).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 2);
        let first: Value = serde_json::from_str(&sent[0].body).unwrap();
        assert!(first["messages"][2].get("reasoning_content").is_none());
        let retry: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert_eq!(retry["messages"][1]["reasoning_content"], "real trace");
        assert_eq!(retry["messages"][2]["reasoning_content"], "");

        // Without tools: the same recovery (the proactive patch never applies
        // here, only the 400-triggered retry does).
        let out = client.chat(&messages, None, &params).unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 4);
        let first: Value = serde_json::from_str(&sent[2].body).unwrap();
        assert!(first["messages"][2].get("reasoning_content").is_none());
        let retry: Value = serde_json::from_str(&sent[3].body).unwrap();
        assert_eq!(retry["messages"][2]["reasoning_content"], "");
    }

    #[test]
    fn reasoning_400_retry_happens_exactly_once() {
        use crate::mcp::mock_server::{respond, CapturedRequest, MockServer};

        // Always-400 mock: the client must not loop, the error surfaces after
        // exactly one corrected retry.
        let handler: crate::mcp::mock_server::Handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, _req: &CapturedRequest| {
                respond(
                    stream,
                    400,
                    "application/json",
                    r#"{"error":{"message":"reasoning_content must be passed back"}}"#,
                    &[],
                )
            },
        );
        let server = MockServer::start(handler);
        let client = LlmClient::new(&server.url_base, "k");
        let params = ChatParams {
            model: "reasoner-v2".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("earlier")];
        let err = client.chat(&messages, None, &params).unwrap_err();
        assert!(err.to_string().contains("400"), "expected HTTP 400, got {err}");
        assert_eq!(server.request_log().len(), 2);
    }

    #[test]
    fn reasoning_400_retries_once_for_streaming_requests() {
        use crate::mcp::mock_server::{write_sse_headers, MockServer};
        use std::io::Write;

        let handler = std::sync::Arc::new(
            |stream: &mut std::net::TcpStream, req: &crate::mcp::mock_server::CapturedRequest| {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let missing = body["messages"]
                    .as_array()
                    .map(|msgs| {
                        msgs.iter().any(|m| {
                            m.get("role").and_then(Value::as_str) == Some("assistant")
                                && m.get("reasoning_content").is_none()
                        })
                    })
                    .unwrap_or(false);
                if missing {
                    crate::mcp::mock_server::respond(
                        stream,
                        400,
                        "application/json",
                        r#"{"error":{"message":"reasoning_content must be passed back"}}"#,
                        &[],
                    )
                } else {
                    write_sse_headers(stream)?;
                    stream.write_all(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n",
                    )?;
                    stream.write_all(b"data: [DONE]\n\n")
                }
            },
        );
        let server = MockServer::start(handler);
        let client = LlmClient::new(&server.url_base, "k");
        let params = ChatParams {
            model: "reasoner-v2".into(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: Some("medium".into()),
            requires_reasoning_passback: false,
        };
        let messages = vec![ChatMessage::user("hi"), ChatMessage::assistant("earlier")];
        let out = client
            .chat_stream(&messages, None, &params, |_| {})
            .unwrap();
        assert_eq!(out.content.as_deref(), Some("ok"));
        let sent = server.request_log();
        assert_eq!(sent.len(), 2);
        let retry: Value = serde_json::from_str(&sent[1].body).unwrap();
        assert_eq!(retry["messages"][1]["reasoning_content"], "");
        assert_eq!(retry["stream"], json!(true));
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

    // A reader that yields a TimedOut io error on the first read stands in for
    // the stream agent's per-read socket timeout firing after the inactivity
    // window with zero bytes (SPEC 6.0.1, Q23).
    struct SilentReader {
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl std::io::Read for SilentReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no data"))
        }
    }

    impl Drop for SilentReader {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn run_sse_aborts_on_inactivity_with_distinct_error_and_closes_reader() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let err = run_sse(
            SilentReader {
                dropped: std::sync::Arc::clone(&dropped),
            },
            DEFAULT_STREAM_INACTIVITY_TIMEOUT,
            &mut |_| {},
        )
        .unwrap_err();
        // Distinct, user-readable error the frontend can offer «Повторить» for.
        assert!(matches!(err, AppError::LlmTimeout(_)), "{err}");
        assert_eq!(err.code(), "llm-timeout");
        let msg = err.to_string();
        assert!(msg.contains("40"), "message names the threshold: {msg}");
        assert!(msg.contains("секунд"), "message is Russian: {msg}");
        // The serialized payload carries the distinct code for the UI.
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], "llm-timeout");
        // The reader (and its socket) is dropped on the early return: cleanup.
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "reader must be dropped when the watchdog aborts"
        );
    }

    // A provider that keeps sending SSE lines, however slowly, must NOT abort:
    // the watchdog is per-read inactivity, not a total deadline. The reader
    // sleeps between reads (total well past the tiny threshold passed in) yet
    // run_sse completes with the full answer.
    struct SlowReader {
        parts: Vec<Vec<u8>>,
        idx: usize,
        gap: Duration,
    }

    impl std::io::Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.idx >= self.parts.len() {
                return Ok(0);
            }
            std::thread::sleep(self.gap);
            let part = &self.parts[self.idx];
            let n = part.len().min(buf.len());
            buf[..n].copy_from_slice(&part[..n]);
            self.idx += 1;
            Ok(n)
        }
    }

    #[test]
    fn run_sse_does_not_abort_a_slow_but_live_stream() {
        let lines = [
            r#"data: {"choices":[{"index":0,"delta":{"content":"При"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"вет"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"!"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        let parts: Vec<Vec<u8>> = lines.iter().map(|l| format!("{l}\n").into_bytes()).collect();
        // 5 reads x 6 ms = 30 ms of live streaming, past a 1 ms threshold.
        let reader = SlowReader {
            parts,
            idx: 0,
            gap: Duration::from_millis(6),
        };
        let mut streamed = Vec::new();
        let outcome = run_sse(reader, Duration::from_millis(1), &mut |e| {
            if let ChatEvent::Delta(t) = e {
                streamed.push(t);
            }
        })
        .expect("slow-but-live stream must not abort");
        assert_eq!(streamed, ["При", "вет", "!"]);
        assert_eq!(outcome.content.as_deref(), Some("Привет!"));
    }

    #[test]
    fn stream_inactivity_timeout_is_injectable() {
        // The test seam lets a caller shrink the watchdog without a real 40 s.
        let client = LlmClient::with_stream_timeout("https://example.test/v1", "k", Duration::from_millis(5));
        assert_eq!(client.stream_timeout, Duration::from_millis(5));
        // The default constructor keeps the shipped 40 s threshold.
        assert_eq!(
            LlmClient::new("https://example.test/v1", "k").stream_timeout,
            DEFAULT_STREAM_INACTIVITY_TIMEOUT
        );
        assert_eq!(DEFAULT_STREAM_INACTIVITY_TIMEOUT, Duration::from_secs(40));
    }

    #[test]
    fn client_from_config_resolves_active_and_falls_back_to_legacy() {
        // Active provider present with a valid (closed) localhost endpoint and an
        // empty legacy baseUrl: building must succeed, proving the active
        // endpoint is the one used.
        let dir = temp_test_dir("llm-client-active");
        write_config(
            &dir,
            ConfigKind::Llm,
            json!({
                "baseUrl": "", "apiKey": "",
                "providers": [
                    { "id": "a", "name": "A", "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "ka" },
                    { "id": "b", "name": "B", "baseUrl": "http://127.0.0.1:8/v1", "apiKey": "kb" }
                ],
                "activeProvider": "b",
            }),
        )
        .unwrap();
        assert!(client_from_config(&dir).is_ok());

        // Dangling active id and empty legacy baseUrl: nothing to build from.
        write_config(
            &dir,
            ConfigKind::Llm,
            json!({
                "baseUrl": "", "apiKey": "",
                "providers": [{ "id": "a", "name": "A", "baseUrl": "http://127.0.0.1:9/v1", "apiKey": "ka" }],
                "activeProvider": "missing",
            }),
        )
        .unwrap();
        assert!(matches!(client_from_config(&dir), Err(AppError::Invalid(_))));

        // Pre-Q-24 file (no providers): legacy baseUrl is the fallback.
        write_config(&dir, ConfigKind::Llm, json!({ "baseUrl": "http://127.0.0.1:7/v1", "apiKey": "k" }))
            .unwrap();
        assert!(client_from_config(&dir).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn client_from_config_rejects_missing_key_for_remote_and_allows_local() {
        let dir = temp_test_dir("llm-client-missing-key");
        write_config(
            &dir,
            ConfigKind::Llm,
            json!({
                "baseUrl": "https://api.openai.com/v1",
                "apiKey": "",
                "providers": [
                    { "id": "a", "name": "OpenAI", "baseUrl": "https://api.openai.com/v1", "apiKey": "" }
                ],
                "activeProvider": "a",
            }),
        )
        .unwrap();
        let err = client_from_config(&dir).err().expect("remote provider without key must fail");
        assert!(err.to_string().contains("API-ключ не задан у активного провайдера"));

        write_config(
            &dir,
            ConfigKind::Llm,
            json!({
                "baseUrl": "http://localhost:11434/v1",
                "apiKey": "",
                "providers": [
                    { "id": "a", "name": "Ollama", "baseUrl": "http://localhost:11434/v1", "apiKey": "" }
                ],
                "activeProvider": "a",
            }),
        )
        .unwrap();
        assert!(client_from_config(&dir).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provider_add_validates_and_seeds_first_active() {
        let dir = temp_test_dir("llm-provider-add");
        assert!(matches!(provider_add(&dir, "", "https://a.test/v1", "k"), Err(AppError::Invalid(_))));
        assert!(matches!(provider_add(&dir, "A", "ftp://a.test", "k"), Err(AppError::Invalid(_))));
        assert!(matches!(provider_add(&dir, "A", "https://a.test/v1", ""), Err(AppError::Invalid(_))));
        // Empty key is allowed for a localhost endpoint (Ollama-style).
        let list = provider_add(&dir, "Local", "http://localhost:11434/v1", "").unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].active, "the first provider becomes active");
        assert!(matches!(
            provider_add(&dir, "Local", "https://b.test/v1", "k"),
            Err(AppError::Invalid(_))
        ));
        let list = provider_add(&dir, "Cloud", "https://api.deepseek.com/v1", "sk-x").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().filter(|p| p.active).count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provider_update_activate_delete_and_mirror_legacy() {
        let dir = temp_test_dir("llm-provider-crud");
        let list = provider_add(&dir, "A", "https://a.test/v1", "ka").unwrap();
        let a_id = list[0].id.clone();
        let list = provider_add(&dir, "B", "https://b.test/v1", "kb").unwrap();
        let b_id = list.iter().find(|p| p.name == "B").unwrap().id.clone();

        // A is active (first). Activating B mirrors B into the legacy fields.
        provider_activate(&dir, &b_id).unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();
        assert_eq!(v["baseUrl"], "https://b.test/v1");
        assert_eq!(v["apiKey"], "kb");
        assert_eq!(v["activeProvider"], b_id);

        // Updating the non-active A leaves the legacy mirror on B.
        provider_update(&dir, &a_id, "A2", "https://a2.test/v1", "ka2").unwrap();
        let v = read_config(&dir, ConfigKind::Llm).unwrap();
        assert_eq!(v["baseUrl"], "https://b.test/v1");

        // Cannot delete the active provider, and cannot delete the last one.
        assert!(matches!(provider_delete(&dir, &b_id), Err(AppError::Invalid(_))));
        let list = provider_delete(&dir, &a_id).unwrap();
        assert_eq!(list.len(), 1);
        assert!(matches!(provider_delete(&dir, &b_id), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provider_list_migrates_legacy_single_provider() {
        let dir = temp_test_dir("llm-provider-list");
        fs::create_dir_all(crate::paths::config_dir(&dir)).unwrap();
        fs::write(
            crate::config::file_path(&dir, ConfigKind::Llm),
            r#"{ "baseUrl": "https://legacy.test/v1", "apiKey": "lk", "model": "m" }"#,
        )
        .unwrap();
        let list = providers_list(&dir).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].active);
        assert_eq!(list[0].base_url, "https://legacy.test/v1");
        fs::remove_dir_all(&dir).unwrap();
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
