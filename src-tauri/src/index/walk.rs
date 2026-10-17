use crate::config::IndexingConfig;
use crate::paths;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub struct WalkedFile {
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

// Recursive walk of a project folder; excluded dirs and app noise files are
// skipped, symlinks are not followed.
pub fn walk(root: &Path, cfg: &IndexingConfig) -> Vec<WalkedFile> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !cfg.exclude_dirs.iter().any(|d| d == &name) {
                    stack.push(path);
                }
            } else if file_type.is_file() && name != ".DS_Store" && name != paths::DB_FILE {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                out.push(WalkedFile {
                    rel,
                    path,
                    size: meta.len(),
                    mtime,
                });
            }
        }
    }
    out
}
