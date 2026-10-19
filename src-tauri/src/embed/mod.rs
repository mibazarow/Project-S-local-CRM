use crate::error::{AppError, Result};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "ort-backend")]
use std::collections::HashMap;
#[cfg(feature = "ort-backend")]
use std::path::PathBuf;
#[cfg(feature = "ort-backend")]
use std::sync::{Mutex, OnceLock};

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

// Process-wide embedder cache: the ONNX session + tokenizer load once per
// model directory and are shared by every search/index call (SPEC <300 ms).
// Keyed by the model dir, which is unique per working dir and backend.
#[cfg(feature = "ort-backend")]
static EMBEDDER_CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<dyn Embedder>>>> = OnceLock::new();

#[cfg(feature = "ort-backend")]
fn embedder_cache() -> &'static Mutex<HashMap<PathBuf, Arc<dyn Embedder>>> {
    EMBEDDER_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// Look up by key; construct once on a miss, share the instance afterwards.
#[cfg(feature = "ort-backend")]
fn cached_embedder<F>(key: PathBuf, factory: F) -> Arc<dyn Embedder>
where
    F: FnOnce() -> Arc<dyn Embedder>,
{
    let mut cache = embedder_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = cache.get(&key) {
        return Arc::clone(existing);
    }
    let embedder = factory();
    cache.insert(key, Arc::clone(&embedder));
    embedder
}

#[cfg(feature = "ort-backend")]
pub fn select_embedder(wd: &Path) -> Arc<dyn Embedder> {
    if let Some(dir) = model_store::resolved_model_dir(wd) {
        cached_embedder(dir.clone(), move || {
            Arc::new(ort_backend::OrtEmbedder::new(dir, EMBED_DIM))
        })
    } else {
        // Noop is cheap to construct, so it stays uncached: the model files
        // can appear between two calls and the next one must pick them up.
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

    #[cfg(feature = "ort-backend")]
    static FAKE_CONSTRUCTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[cfg(feature = "ort-backend")]
    struct CountingEmbedder;

    #[cfg(feature = "ort-backend")]
    impl Embedder for CountingEmbedder {
        fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| vec![t.chars().count() as f32; EMBED_DIM])
                .collect())
        }
        fn dim(&self) -> usize {
            EMBED_DIM
        }
        fn backend_id(&self) -> &'static str {
            "counting"
        }
    }

    #[cfg(feature = "ort-backend")]
    #[test]
    fn embedder_cache_constructs_once_per_key_and_reuses_the_same_instance() {
        use std::sync::atomic::Ordering;
        let key_a = crate::onboarding::temp_test_dir("embed-cache-a").join("model");
        let key_b = crate::onboarding::temp_test_dir("embed-cache-b").join("model");
        let factory = || {
            FAKE_CONSTRUCTIONS.fetch_add(1, Ordering::SeqCst);
            Arc::new(CountingEmbedder) as Arc<dyn Embedder>
        };
        let first = cached_embedder(key_a.clone(), factory);
        let second = cached_embedder(key_a.clone(), factory);
        assert_eq!(FAKE_CONSTRUCTIONS.load(Ordering::SeqCst), 1, "built twice for one key");
        assert!(Arc::ptr_eq(&first, &second), "second call returned a new instance");
        assert_eq!(
            first.embed_texts(&["привет".into()]).unwrap(),
            second.embed_texts(&["привет".into()]).unwrap(),
            "vectors differ across cached calls"
        );
        let other = cached_embedder(key_b, factory);
        assert_eq!(FAKE_CONSTRUCTIONS.load(Ordering::SeqCst), 2, "different key must build");
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[cfg(feature = "ort-backend")]
    #[test]
    fn select_embedder_reuses_the_cached_instance_for_the_same_model_dir() {
        let wd = crate::onboarding::temp_test_dir("embed-select-cache");
        stub_model_files(&wd);
        let first = select_embedder(&wd);
        let second = select_embedder(&wd);
        assert_eq!(first.backend_id(), "ort");
        assert_eq!(first.dim(), EMBED_DIM);
        assert!(Arc::ptr_eq(&first, &second), "select_embedder rebuilt the embedder");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    // Needs the provisioned model in the real working dir
    // (~/Documents/PROJECT-S, PROJECT_S_WD override; provision with
    // `cargo test provision_real_working_dir -- --ignored`):
    //   cargo test embed_cache_timing -- --ignored
    #[cfg(feature = "ort-backend")]
    #[test]
    #[ignore]
    fn embed_cache_timing_second_call_skips_session_reload() {
        use std::time::Instant;
        let wd = model_store::real_working_dir();
        assert!(
            model_store::model_present(&wd),
            "model not provisioned in {}; run `cargo test provision_real_working_dir -- --ignored` first",
            model_store::model_dir(&wd).display()
        );
        let text = [query_prefix("редизайн сайта")];

        // "Before": a fresh embedder per call pays tokenizer+session load on
        // its first embed (the old per-operation select_embedder behavior).
        let started = Instant::now();
        let fresh = Arc::new(ort_backend::OrtEmbedder::new(model_store::model_dir(&wd), EMBED_DIM));
        let cold_vector = fresh.embed_texts(&text).unwrap().remove(0);
        let cold_ms = started.elapsed().as_secs_f64() * 1000.0;

        // "After": cached select_embedder — the load happens once, every
        // later select+embed pair stays warm.
        let started = Instant::now();
        let first = select_embedder(&wd);
        let select1_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        let warm1_vector = first.embed_texts(&text).unwrap().remove(0);
        let warm1_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let second = select_embedder(&wd);
        let select2_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        let warm2_vector = second.embed_texts(&text).unwrap().remove(0);
        let warm2_ms = started.elapsed().as_secs_f64() * 1000.0;

        assert!(Arc::ptr_eq(&first, &second), "second select rebuilt the embedder");
        assert_eq!(warm1_vector, warm2_vector, "cached instance vectors differ");
        // Same model, deterministic run: cached vectors match the fresh ones.
        let dot: f32 = cold_vector.iter().zip(&warm2_vector).map(|(a, b)| a * b).sum();
        assert!((dot - 1.0).abs() < 1e-3, "cached vectors drifted from fresh: {dot}");
        eprintln!(
            "embed cache timing: fresh-per-call load+embed={cold_ms:.0} ms; \
             cached: select#1={select1_us:.0} us + embed={warm1_ms:.0} ms; \
             select#2={select2_us:.0} us + embed={warm2_ms:.0} ms"
        );
        assert!(
            warm2_ms * 4.0 < cold_ms,
            "second cached call not much faster: warm={warm2_ms:.1} ms, cold={cold_ms:.1} ms"
        );
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
