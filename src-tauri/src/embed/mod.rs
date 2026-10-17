use crate::config::EmbeddingConfig;
use crate::error::{AppError, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub mod model_store;

#[cfg(feature = "ort-backend")]
mod ort_backend;

// Storage dimension for every vector (MRL truncation of the model's 768-d).
pub const EMBED_DIM: usize = 256;

pub const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

const PROBE_TOTAL: Duration = Duration::from_millis(2_000);
const EMBED_TOTAL: Duration = Duration::from_secs(120);

pub trait Embedder: Send + Sync {
    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dim(&self) -> usize;
    fn backend_id(&self) -> &'static str;
}

pub struct NoopEmbedder;

impl Embedder for NoopEmbedder {
    fn embed_texts(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(AppError::Invalid("embeddings unavailable".into()))
    }
    fn dim(&self) -> usize {
        EMBED_DIM
    }
    fn backend_id(&self) -> &'static str {
        "noop"
    }
}

pub struct OllamaEmbedder {
    model: String,
    dim: usize,
}

impl OllamaEmbedder {
    pub fn new(model: &str, dim: usize) -> Self {
        OllamaEmbedder {
            model: model.to_owned(),
            dim: dim.clamp(1, EMBED_DIM),
        }
    }
}

impl Embedder for OllamaEmbedder {
    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let response = ureq::post(&format!("{OLLAMA_BASE_URL}/api/embed"))
            .timeout(EMBED_TOTAL)
            .send_json(&body)
            .map_err(|e| AppError::Io(format!("ollama: {e}")))?;
        let value: Value = response
            .into_json()
            .map_err(|e| AppError::Io(format!("ollama: {e}")))?;
        let embeddings = value
            .get("embeddings")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::Io("ollama: no embeddings in response".into()))?;
        if embeddings.len() != texts.len() {
            return Err(AppError::Io(format!(
                "ollama: {} embeddings for {} texts",
                embeddings.len(),
                texts.len()
            )));
        }
        let mut out = Vec::with_capacity(texts.len());
        for embedding in embeddings {
            let floats = embedding
                .as_array()
                .ok_or_else(|| AppError::Io("ollama: embedding is not an array".into()))?;
            let mut vector = floats
                .iter()
                .map(|f| f.as_f64().map(|v| v as f32).unwrap_or(0.0))
                .collect::<Vec<f32>>();
            truncate_renorm(&mut vector, self.dim);
            out.push(vector);
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn backend_id(&self) -> &'static str {
        "ollama"
    }
}

pub fn ollama_models() -> Result<Vec<String>> {
    let response = ureq::get(&format!("{OLLAMA_BASE_URL}/api/tags"))
        .timeout(PROBE_TOTAL)
        .call()
        .map_err(|e| AppError::Io(format!("ollama: {e}")))?;
    let value: Value = response
        .into_json()
        .map_err(|e| AppError::Io(format!("ollama: {e}")))?;
    Ok(value
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

// Prefers the configured backend; anything unavailable becomes Noop so the
// app keeps working on FTS alone.
pub fn select_embedder(cfg: &EmbeddingConfig, wd: &Path) -> Arc<dyn Embedder> {
    if !cfg.enabled {
        return Arc::new(NoopEmbedder);
    }
    match cfg.backend.as_str() {
        "ollama" if ollama_models().is_ok() => Arc::new(OllamaEmbedder::new(&cfg.ollama_model, cfg.dim)),
        "ort" => ort_embedder(cfg, wd),
        _ => Arc::new(NoopEmbedder),
    }
}

#[cfg(feature = "ort-backend")]
fn ort_embedder(cfg: &EmbeddingConfig, wd: &Path) -> Arc<dyn Embedder> {
    if model_store::model_present(wd) {
        Arc::new(ort_backend::OrtEmbedder::new(model_store::model_dir(wd), cfg.dim))
    } else {
        Arc::new(NoopEmbedder)
    }
}

#[cfg(not(feature = "ort-backend"))]
fn ort_embedder(_cfg: &EmbeddingConfig, _wd: &Path) -> Arc<dyn Embedder> {
    Arc::new(NoopEmbedder)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedStatus {
    pub backend: String,
    pub dim: usize,
    pub available: bool,
    pub model_present: bool,
    // Why the backend is unavailable (e.g. missing model files or feature).
    pub reason: Option<String>,
}

pub fn status(cfg: &EmbeddingConfig, wd: &Path) -> EmbedStatus {
    let dim = cfg.dim.clamp(1, EMBED_DIM);
    if !cfg.enabled || cfg.backend == "noop" {
        return EmbedStatus {
            backend: "noop".into(),
            dim,
            available: false,
            model_present: false,
            reason: None,
        };
    }
    if cfg.backend == "ollama" {
        let present = ollama_models()
            .map(|models| {
                models.iter().any(|m| {
                    m == &cfg.ollama_model || m.split(':').next() == Some(cfg.ollama_model.as_str())
                })
            })
            .unwrap_or(false);
        return EmbedStatus {
            backend: "ollama".into(),
            dim,
            available: present,
            model_present: present,
            reason: None,
        };
    }
    if cfg.backend == "ort" {
        let compiled = cfg!(feature = "ort-backend");
        let present = model_store::model_present(wd);
        let reason = if !compiled {
            Some("ort-backend feature not enabled".into())
        } else if !present {
            Some(format!(
                "model files missing in {}",
                model_store::model_dir(wd).display()
            ))
        } else {
            None
        };
        return EmbedStatus {
            backend: "ort".into(),
            dim,
            available: compiled && present,
            model_present: present,
            reason,
        };
    }
    // Unknown backends land here.
    EmbedStatus {
        backend: cfg.backend.clone(),
        dim,
        available: false,
        model_present: false,
        reason: Some(format!("unknown embedding backend: {}", cfg.backend)),
    }
}

// MRL: keep the first `dim` components, then renormalize to unit length.
pub fn truncate_renorm(vector: &mut Vec<f32>, dim: usize) {
    vector.truncate(dim);
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vector.iter_mut() {
            *x /= norm;
        }
    }
}

// SPEC section 2 task prefixes (EmbeddingGemma prompt format).
pub fn query_prefix(query: &str) -> String {
    format!("task: search result | query: {query}")
}

pub fn doc_prefix(name: &str, content: &str) -> String {
    format!("title: {name} | text: {content}")
}

pub fn name_prefix(name: &str) -> String {
    format!("title: {name}")
}

// Search-time blend of the two per-file vectors; missing side counts as 0.
pub fn blend(text: Option<f32>, name: Option<f32>, name_weight: f32) -> f32 {
    let w = name_weight.clamp(0.0, 1.0);
    text.unwrap_or(0.0) * (1.0 - w) + name.unwrap_or(0.0) * w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_renorm_keeps_dim_components_and_unit_norm() {
        let mut v = vec![2.0f32; 768];
        truncate_renorm(&mut v, EMBED_DIM);
        assert_eq!(v.len(), EMBED_DIM);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        let mut zero = vec![0.0f32; 768];
        truncate_renorm(&mut zero, EMBED_DIM);
        assert!(zero.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn prefixes_match_spec() {
        assert_eq!(
            query_prefix("редизайн"),
            "task: search result | query: редизайн"
        );
        assert_eq!(
            doc_prefix("datas.md", "доступ: ftp.example.ru"),
            "title: datas.md | text: доступ: ftp.example.ru"
        );
        assert_eq!(name_prefix("datas.md"), "title: datas.md");
    }

    #[test]
    fn name_weight_03_lets_a_filename_match_win() {
        let name_match = blend(Some(0.5), Some(1.0), 0.3);
        let text_match = blend(Some(0.8), Some(0.0), 0.3);
        assert!(name_match > text_match);
        assert!((name_match - 0.65).abs() < 1e-6);
    }

    #[test]
    fn select_embedder_falls_back_to_noop_without_network_backends() {
        let wd = crate::onboarding::temp_test_dir("embed-select");
        let disabled = EmbeddingConfig {
            enabled: false,
            backend: "ollama".into(),
            ..Default::default()
        };
        assert_eq!(select_embedder(&disabled, &wd).backend_id(), "noop");
        let bogus = EmbeddingConfig {
            backend: "bogus".into(),
            ..Default::default()
        };
        assert_eq!(select_embedder(&bogus, &wd).backend_id(), "noop");
        let noop = EmbeddingConfig {
            backend: "noop".into(),
            ..Default::default()
        };
        assert_eq!(select_embedder(&noop, &wd).backend_id(), "noop");
        // No model files in this temp wd: "ort" falls back to noop with or
        // without the ort-backend feature.
        let ort = EmbeddingConfig {
            backend: "ort".into(),
            ..Default::default()
        };
        assert_eq!(select_embedder(&ort, &wd).backend_id(), "noop");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn ort_status_reports_reason_when_unavailable() {
        let wd = crate::onboarding::temp_test_dir("embed-status");
        let ort = EmbeddingConfig {
            backend: "ort".into(),
            ..Default::default()
        };
        let st = status(&ort, &wd);
        assert_eq!(st.backend, "ort");
        assert!(!st.available);
        assert!(!st.model_present);
        assert!(st.reason.is_some(), "{st:?}");
        let bogus = EmbeddingConfig {
            backend: "bogus".into(),
            ..Default::default()
        };
        assert!(status(&bogus, &wd).reason.is_some());
        std::fs::remove_dir_all(&wd).unwrap();
    }

    // Manual smoke test against a running local Ollama: cargo test -- --ignored
    #[test]
    #[ignore]
    fn live_ollama_embeds_and_renormalizes_to_dim() {
        let wd = crate::onboarding::temp_test_dir("embed-ollama-live");
        let cfg = EmbeddingConfig {
            backend: "ollama".into(),
            ..Default::default()
        };
        let st = status(&cfg, &wd);
        assert!(st.available && st.model_present, "{st:?}");
        let embedder = select_embedder(&cfg, &wd);
        assert_eq!(embedder.backend_id(), "ollama");
        let out = embedder
            .embed_texts(&[query_prefix("привет"), doc_prefix("a.md", "hello")])
            .unwrap();
        assert_eq!(out.len(), 2);
        for v in out {
            assert_eq!(v.len(), EMBED_DIM);
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3);
        }
    }
}
