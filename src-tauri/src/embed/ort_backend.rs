use crate::embed::{truncate_renorm, Embedder, EMBED_DIM};
use crate::error::{AppError, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

// EmbeddingGemma 2 text encoder: 768-d output, 2048-token context.
pub const MODEL_MAX_TOKENS: usize = 2048;
pub const MODEL_OUTPUT_DIM: usize = 768;
// The ONNX export merges image/video/audio tokens into the text stream; for
// text-only inference the three feature inputs are passed as empty tensors.
pub const MULTIMODAL_FEATURE_DIM: usize = 512;

struct Inner {
    tokenizer: tokenizers::Tokenizer,
    // Session::run takes &mut self (ONNX Runtime internals are not thread-safe).
    session: Mutex<ort::session::Session>,
}

// ONNX Runtime allows exactly one environment per process, and ort never
// releases it, so the process-wide cached OrtEmbedder instances share one
// lazily created environment kept alive for the process lifetime.
fn environment() -> std::result::Result<&'static ort::environment::Environment, String> {
    static ENV: OnceLock<std::result::Result<ort::environment::Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| ort::init().build().map_err(|e| format!("ort env: {e}")))
        .as_ref()
        .map_err(|e| e.clone())
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
    let env = environment()?;
    let mut builder = ort::session::Session::builder(env)
        .map_err(|e| format!("ort session builder: {e}"))?;
    // CoreML is a macOS-only provider; every other target keeps the default
    // CPU provider list. See `pin_cpu_only` for why macOS needs the pin.
    #[cfg(target_os = "macos")]
    pin_cpu_only(&mut builder)?;
    // External data (model_quantized.onnx_data) is resolved next to the .onnx.
    let session = builder
        .commit_from_file(model_dir.join("model_quantized.onnx"))
        .map_err(|e| format!("ort model load: {e}"))?;
    Ok(Inner {
        tokenizer,
        session: Mutex::new(session),
    })
}

// The prebuilt macOS ONNX Runtime registers CoreML in its default provider
// list, and CoreML fails on this Q8 QDQ graph ("the ort_value must contain a
// constructed tensor"). Appending CoreML explicitly with
// RequireStaticInputShapes=1 replaces that default registration; since the
// text inputs are dynamic [batch, seq], CoreML then takes only the single
// static-shape constant node and the whole transformer runs on CPU.
//
// Only some macOS ONNX Runtime builds ship the CoreML provider. When this one
// does not, appending it fails with "not supported in this build" — there is
// then no default CoreML registration to replace either, so the default
// providers (CPU) are already what we want. Genuine CoreML errors still fail
// the load; only the unavailable message is downgraded to a warning.
#[cfg(target_os = "macos")]
fn pin_cpu_only(builder: &mut ort::session::builder::SessionBuilder) -> std::result::Result<(), String> {
    use ort::AsPointer as _;
    let name = c"CoreMLExecutionProvider";
    let keys = [c"RequireStaticInputShapes".as_ptr()];
    let values = [c"1".as_ptr()];
    unsafe {
        let base = ort::sys::OrtGetApiBase();
        if base.is_null() {
            return Err("ort api base unavailable".into());
        }
        let api = ((*base).GetApi)(ort::sys::ORT_API_VERSION);
        if api.is_null() {
            return Err("ort api unavailable".into());
        }
        let status = ((*api).SessionOptionsAppendExecutionProvider)(
            builder.ptr_mut(),
            name.as_ptr(),
            keys.as_ptr(),
            values.as_ptr(),
            keys.len(),
        );
        if !(status.0).is_null() {
            let msg = std::ffi::CStr::from_ptr(((*api).GetErrorMessage)(status.0))
                .to_string_lossy()
                .into_owned();
            ((*api).ReleaseStatus)(status.0);
            if coreml_unavailable(&msg) {
                eprintln!("warning: CoreML execution provider is unavailable, using CPU ({msg})");
                return Ok(());
            }
            return Err(format!("ort coreml pin: {msg}"));
        }
    }
    Ok(())
}

// Pure classifier for the "this build has no CoreML provider" reply. Any other
// CoreML failure (bad graph, invalid option, runtime error) stays fatal.
#[cfg(target_os = "macos")]
fn coreml_unavailable(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    const MARKERS: [&str; 3] = [
        "not supported in this build",
        "not available in this build",
        "is not supported on this platform",
    ];
    MARKERS.iter().any(|marker| message.contains(marker))
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
        // Text-only run: the multimodal merge branches get zero image/video/
        // audio tokens via empty [0, 512] feature tensors.
        let empty_features = |name: &str| {
            ort::value::Tensor::from_array((
                [0usize, MULTIMODAL_FEATURE_DIM],
                Vec::<f32>::new().into_boxed_slice(),
            ))
            .map_err(|e| AppError::Io(format!("ort {name}: {e}")))
        };
        let image_features = empty_features("image features")?;
        let video_features = empty_features("video features")?;
        let audio_features = empty_features("audio features")?;
        let mut session = inner
            .session
            .lock()
            .map_err(|_| AppError::Io("ort session lock poisoned".into()))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => ids_tensor,
                "attention_mask" => mask_tensor,
                "image_features" => image_features,
                "video_features" => video_features,
                "audio_features" => audio_features,
            ])
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

    fn model_id(&self) -> String {
        self.model_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
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

    // A macOS runtime built without CoreML must degrade to CPU instead of
    // failing the whole embedder load; real CoreML failures stay fatal.
    #[cfg(target_os = "macos")]
    #[test]
    fn coreml_unavailable_matches_only_build_availability_messages() {
        assert!(coreml_unavailable(
            "CoreMLExecutionProvider execution provider is not supported in this build. \
             Please refer to https://onnxruntime.ai/docs/execution-providers/ for details"
        ));
        assert!(coreml_unavailable(
            "CoreMLExecutionProvider is Not Supported In This Build"
        ));
        assert!(coreml_unavailable(
            "CoreMLExecutionProvider is not supported on this platform"
        ));
        assert!(!coreml_unavailable(
            "the ort_value must contain a constructed tensor"
        ));
        assert!(!coreml_unavailable(
            "CoreMLExecutionProvider: invalid session option RequireStaticInputShapes"
        ));
        assert!(!coreml_unavailable(""));
    }

    // Needs the provisioned model (Settings/models/embeddinggemma-2 in the
    // real app working dir, PROJECT_S_WD override; provision first with
    // `cargo test provision_real_working_dir -- --ignored`):
    // cargo test -- --ignored live_model
    #[test]
    #[ignore]
    fn live_model_embeds_unit_vectors_at_768_dims() {
        use std::time::Instant;
        let wd = crate::embed::model_store::real_working_dir();
        if !crate::embed::model_store::model_present(&wd) {
            crate::embed::model_store::download_text_model(&wd, |name, done, total| {
                eprintln!("{name}: {done}/{total}");
            })
            .unwrap();
        }
        let embedder = OrtEmbedder::new(crate::embed::model_store::model_dir(&wd), EMBED_DIM);
        // With the model files provisioned in the working dir, the always-on
        // selection must resolve to the ort backend.
        let selected = crate::embed::select_embedder(&wd);
        assert_eq!(selected.backend_id(), "ort");
        assert_eq!(selected.dim(), EMBED_DIM);
        let texts = vec![
            crate::embed::query_prefix("редизайн сайта"),
            crate::embed::doc_prefix("redesign.md", "новый дизайн главной страницы сайта"),
            crate::embed::doc_prefix("schet.txt", "счёт на оплату канцелярских товаров"),
        ];
        let started = Instant::now();
        embedder.embed_texts(&texts[..1]).unwrap();
        let load_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut vectors = Vec::with_capacity(texts.len());
        let mut per_text_ms = Vec::with_capacity(texts.len());
        for text in &texts {
            let started = Instant::now();
            vectors.push(
                embedder
                    .embed_texts(std::slice::from_ref(text))
                    .unwrap()
                    .remove(0),
            );
            per_text_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        for v in &vectors {
            assert_eq!(v.len(), EMBED_DIM);
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3);
        }
        // unit vectors: cosine similarity = dot product
        let related: f32 = vectors[0].iter().zip(&vectors[1]).map(|(a, b)| a * b).sum();
        let unrelated: f32 = vectors[0].iter().zip(&vectors[2]).map(|(a, b)| a * b).sum();
        eprintln!(
            "live ort: dim={EMBED_DIM}, load+first={load_ms:.0} ms, per short text: {per_text_ms:?} ms, sim(query, related)={related:.4}, sim(query, unrelated)={unrelated:.4}"
        );
        assert!(
            related > unrelated,
            "related {related:.4} !> unrelated {unrelated:.4}"
        );
        // select_embedder now hands out one cached instance per model dir;
        // a manually built second instance must still reuse the process-wide
        // ort environment and stay deterministic.
        let second = OrtEmbedder::new(crate::embed::model_store::model_dir(&wd), EMBED_DIM);
        let again = second
            .embed_texts(std::slice::from_ref(&texts[0]))
            .unwrap()
            .remove(0);
        let same: f32 = again.iter().zip(&vectors[0]).map(|(a, b)| a * b).sum();
        assert!((same - 1.0).abs() < 1e-3, "second embedder drifted: {same}");
    }
}
