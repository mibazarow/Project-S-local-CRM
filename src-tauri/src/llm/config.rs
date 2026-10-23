use crate::config::{
    migrate_llm_providers, new_provider_id, read_config, write_config, ConfigKind, LlmConfig,
    LlmProvider,
};
use crate::error::{AppError, Result};
use crate::llm::client::LlmClient;
use serde_json::json;
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use serde_json::json;
    use std::fs;

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
}
