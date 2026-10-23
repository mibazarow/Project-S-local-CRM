// Filesystem helpers shared by the import flow: staging suffixes, recursive
// copy, path identity, candidate discovery and SKILL.md validation.

use crate::error::Result;
use crate::paths;
use super::manifest::{manifest_from_frontmatter, split_frontmatter, SkillManifest};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(super) fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// A source holds either one skill (SKILL.md at its root) or several skills
// in its direct subfolders; hidden folders (.git of a clone) are skipped.
pub(crate) fn find_skill_dirs(root: &Path) -> Vec<PathBuf> {
    if root.join(paths::SKILL_FILE).is_file() {
        return vec![root.to_path_buf()];
    }
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !path.is_dir() {
                continue;
            }
            if path.join(paths::SKILL_FILE).is_file() {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    dirs
}

pub(crate) fn validate_skill_dir(dir: &Path) -> std::result::Result<SkillManifest, String> {
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file = dir.join(paths::SKILL_FILE);
    let text = fs::read_to_string(&file).map_err(|e| format!("SKILL.md unreadable: {e}"))?;
    let Some(fm) = split_frontmatter(&text).0 else {
        return Err("frontmatter is missing or malformed (--- fences)".into());
    };
    manifest_from_frontmatter(&fm, &folder, file, false, &HashSet::new())
}
