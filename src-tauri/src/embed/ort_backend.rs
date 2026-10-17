use crate::embed::{truncate_renorm, Embedder, EMBED_DIM};
use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

// EmbeddingGemma 2 text encoder: 768-d output, 2048-token context.
pub const MODEL_MAX_TOKENS: usize = 2048;
pub const MODEL_OUTPUT_DIM: usize = 768;

struct Inner {
    tokenizer: tokenizers::Tokenizer,
    // Session::run takes &mut self (ONNX Runtime internals are not thread-safe).
    session: Mutex<ort::session::Session>,
    _env: ort::environment::Environment,
}

// Loads the tokenizer + ONNX session lazily on the first embed call so
// selecting the backend stays cheap; any failure is returned, never panics.
pub struct OrtEmbedder {
    dim: usize,
    model_dir: PathBuf,
    inner: OnceLock<std::result::Result<Inner, String>>,
}

impl OrtEmbedder {
    pub fn new(model_dir: PathBuf, dim: usize) -> Self {
        OrtEmbedder {
            dim: dim.clamp(1, EMBED_DIM),
            model_dir,
            inner: OnceLock::new(),
        }
    }

    fn inner(&self) -> std::result::Result<&Inner, String> {
        self.inner
            .get_or_init(|| load(&self.model_dir))
            .as_ref()
            .map_err(|e| e.clone())
    }
}

fn load(model_dir: &Path) -> std::result::Result<Inner, String> {
    let mut tokenizer = tokenizers::Tokenizer::from_file(model_dir.join("tokenizer.json"))
        .map_err(|e| format!("tokenizer: {e}"))?;
    // Gemma tokenizer.json carries a post-processor that prepends <bos>, so
    // encode(text, add_special_tokens=true) handles BOS per tokenizer_config.
    tokenizer.with_padding(Some(tokenizers::PaddingParams::default()));
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: MODEL_MAX_TOKENS,
            ..Default::default()
        }))
        .map_err(|e| format!("tokenizer params: {e}"))?;
    let env = ort::init().build().map_err(|e| format!("ort env: {e}"))?;
    // External data (model_quantized.onnx_data) is resolved next to the .onnx.
    let session = ort::session::Session::builder(&env)
        .map_err(|e| format!("ort session builder: {e}"))?
        .commit_from_file(model_dir.join("model_quantized.onnx"))
        .map_err(|e| format!("ort model load: {e}"))?;
    Ok(Inner {
        tokenizer,
        session: Mutex::new(session),
        _env: env,
    })
}

// Mean-pool [seq, hidden] rows over the attention mask, then MRL-truncate to
// `dim` with renormalization (unit L2 norm).
fn pool_row(hidden: &[f32], mask: &[u32], hidden_dim: usize, dim: usize) -> Vec<f32> {
    let mut acc = vec![0.0f32; hidden_dim];
    let mut count = 0.0f32;
    for (s, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        count += 1.0;
        let row = &hidden[s * hidden_dim..(s + 1) * hidden_dim];
        for (a, &v) in acc.iter_mut().zip(row) {
            *a += v;
        }
    }
    if count > 0.0 {
        for a in acc.iter_mut() {
            *a /= count;
        }
    }
    let mut vector = acc;
    truncate_renorm(&mut vector, dim);
    vector
}

impl Embedder for OrtEmbedder {
    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self.inner().map_err(AppError::Io)?;
        let encodings = inner
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| AppError::Io(format!("tokenize: {e}")))?;
        let batch = encodings.len();
        let seq = encodings[0].len();
        let mut input_ids = Vec::with_capacity(batch * seq);
        let mut attention_mask = Vec::with_capacity(batch * seq);
        for encoding in &encodings {
            input_ids.extend(encoding.get_ids().iter().map(|&t| t as i64));
            attention_mask.extend(encoding.get_attention_mask().iter().map(|&m| m as i64));
        }
        let ids_tensor = ort::value::Tensor::from_array(([batch, seq], input_ids.into_boxed_slice()))
            .map_err(|e| AppError::Io(format!("ort input ids: {e}")))?;
        let mask_tensor =
            ort::value::Tensor::from_array(([batch, seq], attention_mask.into_boxed_slice()))
                .map_err(|e| AppError::Io(format!("ort attention mask: {e}")))?;
        let mut session = inner
            .session
            .lock()
            .map_err(|_| AppError::Io("ort session lock poisoned".into()))?;
        let outputs = session
            .run(ort::inputs!["input_ids" => ids_tensor, "attention_mask" => mask_tensor])
            .map_err(|e| AppError::Io(format!("ort run: {e}")))?;
        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| AppError::Io(format!("ort output: {e}")))?;
        // Shape derefs to [i64]
        if shape.len() != 3 || shape[0] as usize != batch || shape[1] as usize != seq {
            return Err(AppError::Io(format!(
                "ort: unexpected output shape {shape:?}, expected [{batch}, {seq}, {MODEL_OUTPUT_DIM}]"
            )));
        }
        let hidden_dim = shape[2] as usize;
        let row_len = seq * hidden_dim;
        if data.len() < batch * row_len {
            return Err(AppError::Io(format!(
                "ort: output has {} floats for shape {shape:?}",
                data.len()
            )));
        }
        let mut out = Vec::with_capacity(batch);
        for (b, encoding) in encodings.iter().enumerate() {
            let start = b * row_len;
            out.push(pool_row(
                &data[start..start + row_len],
                encoding.get_attention_mask(),
                hidden_dim,
                self.dim,
            ));
        }
        Ok(out)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn backend_id(&self) -> &'static str {
        "ort"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_row_masks_pads_and_renormalizes_to_dim() {
        // seq=3, hidden=4: row 2 is padding (mask=0) and must not contribute.
        let hidden = vec![
            1.0, 0.0, 0.0, 0.0, //
            3.0, 0.0, 0.0, 0.0, //
            99.0, 99.0, 99.0, 99.0,
        ];
        let mask = vec![1, 1, 0];
        let v = pool_row(&hidden, &mask, 4, 3);
        assert_eq!(v.len(), 3);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        // mean = [2,0,0,0] -> truncated to 3 dims, unit-normalized = [1,0,0]
        assert!((v[0] - 1.0).abs() < 1e-5);
        assert!(v[1].abs() < 1e-5);
    }

    #[test]
    fn pool_row_all_masked_yields_zero_vector() {
        let hidden = vec![1.0f32, 2.0, 3.0];
        let v = pool_row(&hidden, &[0, 0], 3, 3);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn missing_model_dir_yields_error_not_panic() {
        let embedder = OrtEmbedder::new(PathBuf::from("/nonexistent/embed-model"), EMBED_DIM);
        let err = embedder.embed_texts(&["привет".into()]).unwrap_err();
        assert!(err.to_string().contains("tokenizer"), "{err}");
    }

    // Needs the downloaded model (Settings/models/embeddinggemma-2) in a temp
    // working dir: cargo test --features ort-backend -- --ignored
    #[test]
    #[ignore]
    fn live_model_embeds_unit_vectors_at_256_dims() {
        let wd = crate::onboarding::temp_test_dir("ort-live");
        crate::embed::model_store::download_text_model(&wd, |name, done, total| {
            eprintln!("{name}: {done}/{total}");
        })
        .unwrap();
        let embedder = OrtEmbedder::new(crate::embed::model_store::model_dir(&wd), EMBED_DIM);
        let out = embedder
            .embed_texts(&[
                crate::embed::query_prefix("редизайн сайта"),
                crate::embed::doc_prefix("datas.md", "доступ: ftp.example.ru"),
            ])
            .unwrap();
        assert_eq!(out.len(), 2);
        for v in out {
            assert_eq!(v.len(), EMBED_DIM);
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3);
        }
        std::fs::remove_dir_all(&wd).unwrap();
    }
}
