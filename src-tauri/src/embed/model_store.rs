use crate::error::{AppError, Result};
use crate::paths;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ONNX q8 text-encoder export of google/embeddinggemma-2 (Apache-2.0).
pub const HF_REPO: &str = "onnx-community/embeddinggemma-2-ONNX";
pub const MODEL_DIR_NAME: &str = "embeddinggemma-2";

pub struct ModelFile {
    pub repo_path: &'static str,
    pub local_name: &'static str,
}

// fp16 variants exist upstream but are banned (NaN on CPU); q8 only.
pub const TEXT_MODEL_FILES: [ModelFile; 4] = [
    ModelFile {
        repo_path: "onnx/model_quantized.onnx",
        local_name: "model_quantized.onnx",
    },
    ModelFile {
        repo_path: "onnx/model_quantized.onnx_data",
        local_name: "model_quantized.onnx_data",
    },
    ModelFile {
        repo_path: "tokenizer.json",
        local_name: "tokenizer.json",
    },
    ModelFile {
        repo_path: "tokenizer_config.json",
        local_name: "tokenizer_config.json",
    },
];

pub fn model_dir(wd: &Path) -> PathBuf {
    paths::models_dir(wd).join(MODEL_DIR_NAME)
}

pub fn model_present(wd: &Path) -> bool {
    let dir = model_dir(wd);
    TEXT_MODEL_FILES
        .iter()
        .all(|file| dir.join(file.local_name).is_file())
}

#[derive(Default)]
struct Expected {
    size: Option<u64>,
    sha256: Option<String>,
}

// No overall timeout (model files are ~300 MB); 60 s per socket read.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build()
}

// File sizes + sha256 from the HF tree API (LFS `lfs.oid.sha256`). Best-effort:
// on any failure we warn and continue with unverified downloads.
fn fetch_expected() -> HashMap<&'static str, Expected> {
    let url = format!("https://huggingface.co/api/models/{HF_REPO}/tree/main?recursive=true&blobs=true");
    let entries = match agent()
        .get(&url)
        .timeout(Duration::from_secs(30))
        .call()
    {
        Ok(response) => match response.into_json() {
            Ok(Value::Array(entries)) => entries,
            _ => {
                eprintln!("warning: unexpected model metadata response; downloads will not be sha256-verified");
                return HashMap::new();
            }
        },
        Err(e) => {
            eprintln!("warning: could not fetch model file metadata from Hugging Face ({e}); downloads will not be sha256-verified");
            return HashMap::new();
        }
    };
    let mut out = HashMap::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("file") {
            continue;
        }
        let path = entry.get("path").and_then(Value::as_str).unwrap_or("");
        let Some(known) = TEXT_MODEL_FILES.iter().find(|f| f.repo_path == path) else {
            continue;
        };
        let lfs = entry.get("lfs");
        let size = lfs
            .and_then(|l| l.get("size"))
            .and_then(Value::as_u64)
            .or_else(|| entry.get("size").and_then(Value::as_u64));
        let sha256 = lfs
            .and_then(|l| l.get("oid"))
            .and_then(|oid| oid.get("sha256"))
            .and_then(Value::as_str)
            .map(String::from);
        out.insert(known.repo_path, Expected { size, sha256 });
    }
    out
}

// Downloads the four text-encoder files into Settings/models/embeddinggemma-2/.
// Streams to `<name>.part` and renames atomically, so an interrupted run never
// leaves a half-written model file under its final name; files already present
// with the expected size are skipped (resumable-ish).
pub fn download_text_model(wd: &Path, mut on_progress: impl FnMut(&str, u64, u64)) -> Result<()> {
    let dir = model_dir(wd);
    fs::create_dir_all(&dir)?;
    let expected = fetch_expected();
    for file in TEXT_MODEL_FILES.iter() {
        let dest = dir.join(file.local_name);
        let exp = expected.get(file.repo_path);
        if let Some(size) = exp.and_then(|e| e.size) {
            if fs::metadata(&dest).ok().map(|m| m.len()) == Some(size) {
                on_progress(file.local_name, size, size);
                continue;
            }
        }
        download_one(file, &dest, exp.and_then(|e| e.sha256.as_deref()), &mut on_progress)?;
    }
    Ok(())
}

fn download_one(
    file: &ModelFile,
    dest: &Path,
    sha256: Option<&str>,
    on_progress: &mut impl FnMut(&str, u64, u64),
) -> Result<()> {
    let url = format!("https://huggingface.co/{HF_REPO}/resolve/main/{}", file.repo_path);
    let response = agent()
        .get(&url)
        .call()
        .map_err(|e| AppError::Io(format!("download {url}: {e}")))?;
    let total = response
        .header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok());

    #[cfg(feature = "ort-backend")]
    use sha2::Digest as _;
    #[cfg(feature = "ort-backend")]
    let mut hasher = sha2::Sha256::new();

    let tmp = dest.with_extension("part");
    let mut reader = response.into_reader();
    let mut out = fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        #[cfg(feature = "ort-backend")]
        sha2::Sha256::update(&mut hasher, &buf[..n]);
        out.write_all(&buf[..n])?;
        done += n as u64;
        on_progress(file.local_name, done, total.unwrap_or(done));
    }
    out.flush()?;
    drop(out);

    #[cfg(feature = "ort-backend")]
    if let Some(want) = sha256 {
        let got = format!("{:x}", sha2::Sha256::finalize(hasher));
        if got != want {
            let _ = fs::remove_file(&tmp);
            return Err(AppError::Io(format!(
                "{}: sha256 mismatch (got {got}, want {want})",
                file.local_name
            )));
        }
    }
    #[cfg(not(feature = "ort-backend"))]
    if sha256.is_some() {
        eprintln!(
            "warning: sha256 verification of {} skipped (ort-backend feature not enabled)",
            file.local_name
        );
    }
    if let Some(size) = total {
        if done != size {
            let _ = fs::remove_file(&tmp);
            return Err(AppError::Io(format!(
                "{}: got {done} bytes, expected {size}",
                file.local_name
            )));
        }
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;

    #[test]
    fn model_dir_is_under_settings_models() {
        let dir = model_dir(Path::new("/tmp/wd"));
        assert_eq!(dir, PathBuf::from("/tmp/wd/Settings/models/embeddinggemma-2"));
    }

    #[test]
    fn model_present_requires_all_four_files() {
        let wd = temp_test_dir("model-store");
        let dir = model_dir(&wd);
        fs::create_dir_all(&dir).unwrap();
        assert!(!model_present(&wd));
        for (i, file) in TEXT_MODEL_FILES.iter().enumerate() {
            fs::write(dir.join(file.local_name), "x").unwrap();
            if i + 1 < TEXT_MODEL_FILES.len() {
                assert!(!model_present(&wd));
            }
        }
        assert!(model_present(&wd));
        fs::remove_dir_all(&wd).unwrap();
    }

    // Needs network + ~350 MB of downloads: cargo test -- --ignored
    #[test]
    #[ignore]
    fn live_download_provisions_all_text_model_files() {
        let wd = temp_test_dir("model-store-live");
        download_text_model(&wd, |name, done, total| {
            eprintln!("{name}: {done}/{total}");
        })
        .unwrap();
        assert!(model_present(&wd));
        fs::remove_dir_all(&wd).unwrap();
    }
}
