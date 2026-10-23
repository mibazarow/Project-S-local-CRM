use crate::config::{read_config, write_config, ConfigKind, LlmConfig};
use crate::error::{AppError, Result};
use crate::llm::client::LlmClient;
use crate::llm::config::ChatParams;
use crate::llm::types::{ChatMessage, ContentPart};
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTest {
    pub ok: bool,
    pub message: String,
    pub model_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    // Maximum context window reported by the provider, when present.
    pub max_context: Option<u64>,
}

pub(crate) fn parse_models(body: &Value) -> Result<Vec<ModelInfo>> {
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Json("models response has no \"data\" array".into()))?;
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(|id| (id, m)))
        .map(|(id, model)| ModelInfo {
            id: id.to_string(),
            max_context: model_max_context(model),
        })
        .collect())
}

const CONTEXT_LIMIT_KEYS: [&str; 14] = [
    "context_length",
    "max_context_length",
    "context_window",
    "context_window_tokens",
    "max_model_len",
    "max_input_tokens",
    "max_input_token",
    "input_token_limit",
    "max_prompt_tokens",
    "max_context_tokens",
    "max_context_size",
    "context_size",
    "max_sequence_length",
    "max_seq_len",
];

fn model_max_context(model: &Value) -> Option<u64> {
    fn nested_value(value: &Value) -> Option<u64> {
        let obj = value.as_object()?;
        ["tokens", "value", "max", "limit"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(context_number))
    }

    fn walk(value: &Value) -> Option<u64> {
        match value {
            Value::Object(obj) => {
                let mut best = None;
                for (key, val) in obj {
                    let normalized = key.to_ascii_lowercase();
                    let found = if CONTEXT_LIMIT_KEYS.contains(&normalized.as_str()) {
                        context_number(val).or_else(|| nested_value(val))
                    } else {
                        None
                    };
                    let found = found.or_else(|| match val {
                        Value::Object(_) | Value::Array(_) => walk(val),
                        _ => None,
                    });
                    if let Some(tokens) = found {
                        best = Some(best.map_or(tokens, |b: u64| b.max(tokens)));
                    }
                }
                best
            }
            Value::Array(items) => items.iter().filter_map(walk).max(),
            _ => None,
        }
    }

    walk(model)
}

fn context_number(value: &Value) -> Option<u64> {
    if let Some(n) = value.as_u64() {
        return (n > 0).then_some(n);
    }
    let text = value.as_str()?.trim().replace(['_', ',', ' '], "");
    if text.is_empty() {
        return None;
    }
    let (digits, multiplier) = match text.chars().last()?.to_ascii_lowercase() {
        'k' => (&text[..text.len() - 1], 1_000u64),
        'm' => (&text[..text.len() - 1], 1_000_000u64),
        _ => (text.as_str(), 1u64),
    };
    let n: f64 = digits.parse().ok()?;
    (n > 0.0).then(|| (n * multiplier as f64).round() as u64)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use serde_json::json;
    use std::fs;

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
    fn parse_models_extracts_ids_and_rejects_bad_shape() {
        let models = parse_models(&json!({ "data": [{ "id": "gpt-x" }, { "id": "gpt-y" }, { "name": 7 }] }))
            .unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["gpt-x", "gpt-y"]
        );
        assert!(models.iter().all(|m| m.max_context.is_none()));
        assert!(matches!(
            parse_models(&json!({ "error": { "message": "nope" } })),
            Err(AppError::Json(_))
        ));
    }

    #[test]
    fn parse_models_detects_context_limits_from_common_fields_and_metadata() {
        let models = parse_models(&json!({
            "data": [
                { "id": "a", "context_length": 131072, "max_tokens": 8192 },
                { "id": "b", "max_model_len": "32768" },
                { "id": "c", "metadata": { "context_window": 65536 } },
                { "id": "d", "top_provider": { "context_length": 200000 }, "limits": { "max_input_tokens": 180000 } },
                { "id": "e", "context_window": { "tokens": 1000000 } },
                { "id": "f", "max_completion_tokens": 4096 },
            ]
        }))
        .unwrap();
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap().max_context;
        assert_eq!(by_id("a"), Some(131072));
        assert_eq!(by_id("b"), Some(32768));
        assert_eq!(by_id("c"), Some(65536));
        assert_eq!(by_id("d"), Some(200000));
        assert_eq!(by_id("e"), Some(1_000_000));
        assert_eq!(by_id("f"), None);
    }
}
