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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use std::os::unix::fs::symlink;

    fn cfg(exclude: &[&str]) -> IndexingConfig {
        IndexingConfig {
            exclude_dirs: exclude.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn rels(root: &Path, exclude: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = walk(root, &cfg(exclude))
            .into_iter()
            .map(|f| f.rel)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn walk_lists_nested_files_with_root_relative_paths() {
        let root = temp_test_dir("walk-basic");
        fs::create_dir_all(root.join("sub/deep")).unwrap();
        fs::write(root.join("a.md"), "a").unwrap();
        fs::write(root.join("sub/b.md"), "bb").unwrap();
        fs::write(root.join("sub/deep/c.txt"), "ccc").unwrap();
        assert_eq!(rels(&root, &[]), ["a.md", "sub/b.md", "sub/deep/c.txt"]);
        let files = walk(&root, &cfg(&[]));
        let c = files.iter().find(|f| f.rel == "sub/deep/c.txt").unwrap();
        assert_eq!(c.size, 3);
        assert!(c.mtime > 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn walk_skips_excluded_dirs_ds_store_and_the_app_db_file() {
        let root = temp_test_dir("walk-excl");
        fs::create_dir_all(root.join("vendor/inner")).unwrap();
        fs::create_dir_all(root.join("keep")).unwrap();
        fs::write(root.join("vendor/skip.md"), "no").unwrap();
        fs::write(root.join("vendor/inner/deep.md"), "no").unwrap();
        fs::write(root.join(".DS_Store"), "junk").unwrap();
        fs::write(root.join("keep/.DS_Store"), "junk").unwrap();
        fs::write(root.join(paths::DB_FILE), "db").unwrap();
        fs::write(root.join("keep/ok.md"), "yes").unwrap();
        // Hidden dotfiles other than .DS_Store are NOT excluded by default:
        // pinning the current behavior.
        fs::write(root.join(".hidden.txt"), "dot").unwrap();
        assert_eq!(rels(&root, &["vendor"]), [".hidden.txt", "keep/ok.md"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn walk_does_not_follow_or_list_symlinks() {
        let root = temp_test_dir("walk-symlink");
        let outside = temp_test_dir("walk-symlink-outside");
        fs::write(outside.join("target.md"), "out").unwrap();
        fs::create_dir_all(outside.join("dir")).unwrap();
        fs::write(outside.join("dir/inside.md"), "in").unwrap();
        fs::write(root.join("real.md"), "real").unwrap();
        symlink(outside.join("target.md"), root.join("link.md")).unwrap();
        symlink(&outside, root.join("link-dir")).unwrap();
        // Only the real file surfaces: symlinked files and dirs are neither
        // listed nor descended into.
        assert_eq!(rels(&root, &[]), ["real.md"]);
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn walk_of_missing_root_yields_nothing() {
        let missing = temp_test_dir("walk-missing").join("nope");
        assert!(walk(&missing, &cfg(&[])).is_empty());
    }
}
