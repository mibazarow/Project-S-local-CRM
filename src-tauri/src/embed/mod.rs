use crate::error::{AppError, Result};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

pub mod model_store;

#[cfg(feature = "ort-backend")]
mod ort_backend;

// Storage dimension for every vector: the full EmbeddingGemma 2 output
// (768-d, no MRL truncation — maximum quality is always on).
pub const EMBED_DIM: usize = 768;

// Fixed blend weight of the filename vector in hybrid scoring (best value
// from tuning; not user-configurable).
pub const NAME_WEIGHT: f32 = 0.3;

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

// Semantic search is always on: the in-app ORT model (EmbeddingGemma 2 q8)
// is the only backend. Noop stays as the internal fallback for when the
// model files are physically missing (or the ort-backend feature is not
// compiled), so the app keeps working on FTS alone.
#[cfg(feature = "ort-backend")]
pub fn select_embedder(wd: &Path) -> Arc<dyn Embedder> {
    if model_store::model_present(wd) {
        Arc::new(ort_backend::OrtEmbedder::new(model_store::model_dir(wd), EMBED_DIM))
    } else {
        eprintln!(
            "embedding: model files are missing in {}; falling back to noop (FTS only). Provision them with `cargo test provision_real_working_dir -- --ignored`.",
            model_store::model_dir(wd).display()
        );
        Arc::new(NoopEmbedder)
    }
}

#[cfg(not(feature = "ort-backend"))]
pub fn select_embedder(_wd: &Path) -> Arc<dyn Embedder> {
    eprintln!(
        "embedding: the ort-backend feature is not compiled; falling back to noop (FTS only)"
    );
    Arc::new(NoopEmbedder)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedStatus {
    pub backend: String,
    pub dim: usize,
    pub available: bool,
    pub model_present: bool,
    // Why the model is unavailable (missing files or feature not compiled).
    pub reason: Option<String>,
}

// Always reports "ort" when the model files are on disk and the feature is
// compiled; "noop" only as the internal fallback.
pub fn status(wd: &Path) -> EmbedStatus {
    let compiled = cfg!(feature = "ort-backend");
    let present = model_store::model_present(wd);
    if compiled && present {
        EmbedStatus {
            backend: "ort".into(),
            dim: EMBED_DIM,
            available: true,
            model_present: true,
            reason: None,
        }
    } else {
        let reason = if !compiled {
            Some("ort-backend feature not enabled".into())
        } else {
            Some(format!(
                "model files missing in {}",
                model_store::model_dir(wd).display()
            ))
        };
        EmbedStatus {
            backend: "noop".into(),
            dim: EMBED_DIM,
            available: false,
            model_present: present,
            reason,
        }
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

    fn stub_model_files(wd: &Path) {
        let dir = model_store::model_dir(wd);
        std::fs::create_dir_all(&dir).unwrap();
        for file in model_store::TEXT_MODEL_FILES.iter() {
            std::fs::write(dir.join(file.local_name), "x").unwrap();
        }
    }

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
        let name_match = blend(Some(0.5), Some(1.0), NAME_WEIGHT);
        let text_match = blend(Some(0.8), Some(0.0), NAME_WEIGHT);
        assert!(name_match > text_match);
        assert!((name_match - 0.65).abs() < 1e-6);
    }

    #[test]
    fn select_embedder_is_noop_only_while_model_files_are_missing() {
        let wd = crate::onboarding::temp_test_dir("embed-select");
        assert_eq!(select_embedder(&wd).backend_id(), "noop");
        stub_model_files(&wd);
        #[cfg(feature = "ort-backend")]
        assert_eq!(select_embedder(&wd).backend_id(), "ort");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn status_reports_ort_when_model_present_else_noop_with_reason() {
        let wd = crate::onboarding::temp_test_dir("embed-status");
        let missing = status(&wd);
        assert_eq!(missing.backend, "noop");
        assert!(!missing.available);
        assert!(!missing.model_present);
        assert!(missing.reason.is_some(), "{missing:?}");
        assert_eq!(missing.dim, EMBED_DIM);

        stub_model_files(&wd);
        let present = status(&wd);
        assert!(present.model_present);
        assert_eq!(present.dim, EMBED_DIM);
        assert_eq!(present.available, cfg!(feature = "ort-backend"));
        assert_eq!(
            present.backend,
            if cfg!(feature = "ort-backend") { "ort" } else { "noop" }
        );
        std::fs::remove_dir_all(&wd).unwrap();
    }
}
