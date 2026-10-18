// Shared path policy (refactor S1.6): the single implementation of the
// primitives behind every path resolution in the app — project file ops
// (fs_ops), the Notes vault (notes/mod.rs), and the harness/project agent
// tools. Component normalization, lenient Unicode name matching,
// containment, symlink guards, and the resolve-existing / resolve-for-
// mutation stacks live here so they cannot drift apart. The call-site
// difference stays explicit in the parameters: project and Notes resolve
// root-relative paths with strict containment (contain_existing /
// contain_for_mutation); harness tools resolve absolute, ~-expanded or
// working-dir-relative paths lexically (resolve_path) and layer their own
// territory and symlink guards (first_symlink_below / is_symlink) on top.
use crate::error::{AppError, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// Lexical normalization and harness-style path expansion
// ---------------------------------------------------------------------------

// Expands "~", joins relative paths onto the working dir, then collapses
// "." and ".." lexically (the target may not exist yet, e.g. fs_write).
pub(crate) fn resolve_path(raw: &str, wd: &Path) -> PathBuf {
    let expanded = if raw == "~" {
        home_dir()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir().join(rest)
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        wd.join(raw)
    };
    normalize(&expanded)
}

pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// Best-effort canonical form for policy checks: existing prefixes resolve
// through symlinks, the missing tail (fs_write targets) is appended back.
pub(crate) fn canonicalish(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    loop {
        match cur.canonicalize() {
            Ok(c) => {
                let mut out = c;
                while let Some(name) = tail.pop() {
                    out.push(name);
                }
                return out;
            }
            Err(_) => match (cur.parent(), cur.file_name()) {
                (Some(p), Some(name)) => {
                    tail.push(name.to_os_string());
                    cur = p;
                }
                _ => return normalize(path),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

// Strict containment: `path` is at or below `root`. Callers canonicalize
// both sides first whenever symlink hops must be caught.
pub(crate) fn within(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

// ---------------------------------------------------------------------------
// Lenient name resolution (SPEC: unreliable reads of names with NBSP, em
// dash, №, NFD, case variants). Exact names always win; a miss is re-matched
// component by component against the real directory entries.
// ---------------------------------------------------------------------------

// Folds a name for lenient comparison: trims, normalizes to NFC (so NFC and
// NFD forms compare equal), lowercases, maps dash variants (‐ ‒ – — ─ − and
// soft hyphen) to '-', drops zero-width characters and maps every other
// Unicode space (NBSP, thin, narrow, ideographic, …) to a plain space.
pub(crate) fn fold_name(raw: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().nfc() {
        match ch {
            '\u{00ad}' | '\u{2010}'..='\u{2015}' | '\u{2043}' | '\u{2212}' => out.push('-'),
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' => {}
            c if c.is_whitespace() => out.push(' '),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

// Character-level Levenshtein distance, used only to suggest near names.
fn levenshtein(a: &str, b: &str) -> usize {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=bc.len()).collect();
    let mut cur = vec![0usize; bc.len() + 1];
    for i in 1..=ac.len() {
        cur[0] = i;
        for j in 1..=bc.len() {
            let cost = usize::from(ac[i - 1] != bc[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bc.len()]
}

// "Close enough to be worth suggesting" for the not-found hint.
fn is_close(a: &str, b: &str) -> bool {
    let longest = a.chars().count().max(b.chars().count());
    longest > 0 && levenshtein(a, b) <= (longest / 3).max(2)
}

// Resolves one component inside `dir`: the exact name first (symlinks pass
// through; callers canonicalize/refuse later), then a lenient match against
// the real entries. A unique match returns the ACTUAL on-disk path; several
// matches are refused with the candidate list (never guessed); none is an
// error naming the nearest candidates as a hint.
fn resolve_component(dir: &Path, name: &OsStr, rel: &str, label: &str) -> Result<PathBuf> {
    let exact = dir.join(name);
    if fs::symlink_metadata(&exact).is_ok() {
        return Ok(exact);
    }
    let folded = fold_name(&name.to_string_lossy());
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut near: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for item in rd.flatten() {
            let entry_name = item.file_name().to_string_lossy().into_owned();
            let entry_folded = fold_name(&entry_name);
            if entry_folded == folded {
                matches.push(item.path());
            } else if is_close(&entry_folded, &folded) {
                near.push(entry_name);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap_or(exact)),
        0 => {
            near.sort();
            near.truncate(5);
            let hint = if near.is_empty() {
                String::new()
            } else {
                let list = near
                    .iter()
                    .map(|n| format!("\"{n}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let tool = if label == "project" { "list_tree" } else { "the folder listing" };
                format!("; nearest names here: {list}; use {tool} for exact names")
            };
            Err(AppError::Invalid(format!(
                "\"{rel}\" not found in the {label} folder{hint}"
            )))
        }
        _ => {
            let mut names: Vec<String> = matches
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            names.sort();
            let list = names
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ");
            Err(AppError::Invalid(format!(
                "\"{rel}\" is ambiguous in the {label} folder: {list} all match; use one exact name"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Root-relative resolution (project folders and the Notes vault)
// ---------------------------------------------------------------------------

pub(crate) fn join_rel(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for part in Path::new(rel).components() {
        match part {
            Component::Normal(name) => out.push(name),
            _ => {
                return Err(AppError::Invalid(format!(
                    "path \"{rel}\" is not project-relative"
                )))
            }
        }
    }
    Ok(out)
}

// Lenient per-component resolution of a root-relative path: returns the
// actual on-disk path; exact components pass through untouched.
pub(crate) fn lenient_join(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let mut cur = root.to_path_buf();
    for part in Path::new(rel).components() {
        match part {
            Component::Normal(name) => cur = resolve_component(&cur, name, rel, label)?,
            _ => {
                return Err(AppError::Invalid(format!(
                    "path \"{rel}\" is not project-relative"
                )))
            }
        }
    }
    Ok(cur)
}

// join_rel + lenient fallback: the resolved (not yet canonicalized) target
// of a root-relative path, for callers that must inspect the pre-canonical
// path (symlink refusal) before canonicalizing.
pub(crate) fn resolve_rel(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let joined = join_rel(root, rel)?;
    if fs::symlink_metadata(&joined).is_ok() {
        return Ok(joined);
    }
    lenient_join(root, rel, label)
}

// Canonical root plus the canonical target of rel_path inside it, rejecting
// absolute paths, "..", and symlink escapes (resolved target must stay
// inside). `label` names the containing folder in errors ("project", "Notes").
// Missing exact names go through the lenient resolver (see lenient_join), so
// NBSP/em-dash/case/NFD variants of an existing entry resolve to the real
// on-disk name instead of failing.
pub(crate) fn contain_existing(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let joined = resolve_rel(root, rel, label)?;
    let canon = joined
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("\"{rel}\" not found")))?;
    if !within(root, &canon) {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" escapes the {label} folder"
        )));
    }
    Ok(canon)
}

// Like contain_existing, but refuses symlink entries themselves, for
// rename/duplicate/delete/move style mutations.
pub(crate) fn contain_for_mutation(root: &Path, rel: &str, label: &str) -> Result<PathBuf> {
    let joined = resolve_rel(root, rel, label)?;
    reject_symlink(&joined, rel)?;
    let canon = joined
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("\"{rel}\" not found")))?;
    if !within(root, &canon) {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" escapes the {label} folder"
        )));
    }
    Ok(canon)
}

// ---------------------------------------------------------------------------
// Lenient resolution of absolute paths (harness tools)
// ---------------------------------------------------------------------------

// Lenient per-component resolution of an absolute, lexically normalized
// path (harness tools): every component is matched exactly first, then
// leniently against the real entries of its parent folder.
pub(crate) fn lenient_absolute(path: &Path) -> Result<PathBuf> {
    let mut cur = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::Prefix(_) => cur.push(part.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                cur.pop();
            }
            Component::Normal(name) => {
                cur = resolve_component(&cur, name, &path.display().to_string(), "folder")?
            }
        }
    }
    Ok(cur)
}

// Re-anchors the parent folders of an absolute path leniently, keeping the
// final component exactly as requested (create/write targets whose own name
// may be new). A truly missing parent chain is returned unchanged so the
// caller can create it; an ambiguous parent folder is refused.
pub(crate) fn lenient_parent(path: &Path) -> Result<PathBuf> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(path.to_path_buf());
    };
    let resolved = if fs::symlink_metadata(parent).is_ok() {
        parent.to_path_buf()
    } else {
        match lenient_absolute(parent) {
            Ok(p) => p,
            Err(e) if e.to_string().contains("ambiguous") => return Err(e),
            Err(_) => parent.to_path_buf(),
        }
    };
    Ok(match path.file_name() {
        Some(name) => resolved.join(name),
        None => resolved,
    })
}

// ---------------------------------------------------------------------------
// Symlink guards
// ---------------------------------------------------------------------------

// True when the final component of `path` exists as a symlink (dangling
// links included, which exists() misses).
pub(crate) fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

// Refuses symlink entries for mutating operations.
pub(crate) fn reject_symlink(joined: &Path, rel: &str) -> Result<()> {
    if is_symlink(joined) {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" is a symlink; symlink operations are not supported"
        )));
    }
    Ok(())
}

// First symlink component on the way to `path` below `base` (the final
// component included), so auto-created parents and in-territory links can
// never land outside the folder the policy checked. None when `path` is not
// below `base` (callers decide what to do with outside paths).
pub(crate) fn first_symlink_below(base: &Path, path: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(base).ok()?;
    let mut cur = base.to_path_buf();
    for part in rest.components() {
        cur.push(part.as_os_str());
        if is_symlink(&cur) {
            return Some(cur);
        }
    }
    None
}

// Refuses a path when any existing component below root is a symlink, so a
// target created through auto-created parents cannot land outside root.
pub(crate) fn reject_symlink_chain(root: &Path, joined: &Path, rel: &str) -> Result<()> {
    if first_symlink_below(root, joined).is_some() {
        return Err(AppError::Invalid(format!(
            "\"{rel}\" is a symlink; symlink operations are not supported"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared accept/reject corpus (S1.6): every case must resolve identically
// through the path_policy primitives and through the old call sites
// (fs_ops re-exports, the Notes mutation resolver, the harness lenient
// resolver), so the stacks cannot drift apart silently.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;

    // The corpus tree: lenient-matchable names, an ambiguous pair, and
    // symlinks pointing outside the root.
    fn build_tree(wd: &Path) -> PathBuf {
        let root = wd.join("root");
        fs::create_dir_all(root.join("inner")).unwrap();
        fs::write(root.join("inner/real file.txt"), "x").unwrap();
        fs::write(root.join("Договор № 017-2026\u{a0}— копия.docx"), "doc").unwrap();
        fs::write(root.join("a\u{2014}b.txt"), "em").unwrap();
        fs::write(root.join("a\u{2013}b.txt"), "en").unwrap();
        let outside = wd.join("outside");
        fs::create_dir_all(outside.join("secret")).unwrap();
        fs::write(outside.join("secret/leak.txt"), "leak").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("dirlink")).unwrap();
            std::os::unix::fs::symlink(outside.join("secret/leak.txt"), root.join("filelink"))
                .unwrap();
        }
        root
    }

    // (rel, Some(real sub-path below root) | None = rejected)
    fn corpus() -> Vec<(&'static str, Option<&'static str>)> {
        vec![
            ("", Some("")),
            ("inner", Some("inner")),
            ("inner/real file.txt", Some("inner/real file.txt")),
            // lenient: case folding
            ("INNER/REAL FILE.TXT", Some("inner/real file.txt")),
            // lenient: NBSP in the query folds to the real space
            ("inner/real\u{a0}file.txt", Some("inner/real file.txt")),
            // lenient: NBSP+em-dash on disk vs plain hyphen+spaces in the query
            (
                "Договор № 017-2026 - копия.docx",
                Some("Договор № 017-2026\u{a0}— копия.docx"),
            ),
            // ambiguous lenient match is refused with the candidates, never guessed
            ("a-b.txt", None),
            ("missing.txt", None),
            // traversal is refused lexically
            ("..", None),
            ("../x", None),
            ("/etc/passwd", None),
            ("a/../../x", None),
            ("./x", None),
            ("inner/./x", None),
            ("inner/../inner", None),
            // symlink escapes are refused after canonicalization
            #[cfg(unix)]
            ("dirlink", None),
            #[cfg(unix)]
            ("dirlink/secret/leak.txt", None),
            #[cfg(unix)]
            ("filelink", None),
        ]
    }

    fn check_read(resolver: impl Fn(&Path, &str, &str) -> Result<PathBuf>, root: &Path, tag: &str) {
        for (rel, want) in corpus() {
            let got = resolver(root, rel, "test");
            match want {
                Some(sub) => {
                    let expected = if sub.is_empty() {
                        root.to_path_buf()
                    } else {
                        root.join(sub).canonicalize().unwrap()
                    };
                    assert_eq!(
                        got.unwrap_or_else(|e| panic!("{tag}: {rel:?} rejected: {e}")),
                        expected,
                        "{tag}: {rel:?}"
                    );
                }
                None => assert!(got.is_err(), "{tag} accepted {rel:?}: {got:?}"),
            }
        }
    }

    #[test]
    fn corpus_read_identical_in_new_module_and_old_call_site() {
        let wd = temp_test_dir("pp-corpus-read");
        let root = build_tree(&wd).canonicalize().unwrap();
        check_read(contain_existing, &root, "path_policy::contain_existing");
        check_read(
            crate::fs_ops::contain_existing,
            &root,
            "fs_ops::contain_existing",
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn corpus_mutation_identical_in_new_module_and_notes_call_site() {
        let wd = temp_test_dir("pp-corpus-mut");
        let root = build_tree(&wd).canonicalize().unwrap();
        check_read(
            contain_for_mutation,
            &root,
            "path_policy::contain_for_mutation",
        );
        // notes keeps its own empty-rel refusal in front of the shared resolver
        for (rel, want) in corpus() {
            let got = crate::notes::existing_for_mutation_at(&root, rel);
            match (want, rel.is_empty()) {
                (Some(_), true) => assert!(got.is_err(), "notes accepted the empty rel"),
                (Some(sub), false) => {
                    let expected = root.join(sub).canonicalize().unwrap();
                    assert_eq!(
                        got.unwrap_or_else(|e| panic!("notes: {rel:?} rejected: {e}")),
                        expected,
                        "notes: {rel:?}"
                    );
                }
                (None, _) => assert!(got.is_err(), "notes mutation accepted {rel:?}: {got:?}"),
            }
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn corpus_harness_pipeline_identical_in_new_module_and_old_call_site() {
        // the harness pipeline is resolve_path (lexical) then lenient_existing;
        // every corpus path goes through both the new primitives and the old
        // harness_tools entry points and must agree
        let wd = temp_test_dir("pp-corpus-harness");
        let root = build_tree(&wd).canonicalize().unwrap();
        for (rel, _) in corpus() {
            let raw = crate::agent::harness_tools::resolve_path(rel, &root);
            assert_eq!(raw, resolve_path(rel, &root), "resolve_path: {rel:?}");
            let old = crate::agent::harness_tools::lenient_existing(&raw);
            let new = lenient_absolute(&raw);
            match (old, new) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "lenient: {rel:?}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "lenient: {rel:?}"),
                (a, b) => panic!("lenient split for {rel:?}: {:?} vs {:?}", a.is_ok(), b.is_ok()),
            }
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn resolve_path_expansion_contract() {
        let wd = PathBuf::from("/wd");
        assert_eq!(resolve_path("a/b", &wd), PathBuf::from("/wd/a/b"));
        assert_eq!(resolve_path("./a/../b", &wd), PathBuf::from("/wd/b"));
        assert_eq!(resolve_path("/x/y", &wd), PathBuf::from("/x/y"));
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert_eq!(resolve_path("~/notes.md", &wd), home.join("notes.md"));
        assert_eq!(resolve_path("~", &wd), home);
        assert_eq!(
            crate::agent::harness_tools::resolve_path("./a/../b", &wd),
            resolve_path("./a/../b", &wd)
        );
    }

    #[cfg(unix)]
    #[test]
    fn mutation_guard_closes_the_symlinked_parent_escape() {
        // The pre-S1.6 Notes mutation resolver (resolve_rel + reject_symlink
        // + canonicalize, without the containment check) accepted a hop out
        // of the vault through a symlinked parent; contain_for_mutation
        // refuses it, exactly like the project stack always did.
        let wd = temp_test_dir("pp-mut-escape");
        let root = build_tree(&wd).canonicalize().unwrap();
        let joined = resolve_rel(&root, "dirlink/secret/leak.txt", "Notes").unwrap();
        reject_symlink(&joined, "rel").unwrap();
        let old_style = joined.canonicalize().unwrap();
        assert!(!within(&root, &old_style), "the escape the old code returned");
        assert!(contain_for_mutation(&root, "dirlink/secret/leak.txt", "Notes").is_err());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn symlink_guards_flag_links_below_base_only() {        let wd = temp_test_dir("pp-corpus-links");
        let root = build_tree(&wd).canonicalize().unwrap();
        assert!(first_symlink_below(&root, &root.join("inner/real file.txt")).is_none());
        assert!(reject_symlink_chain(&root, &root.join("inner/new.txt"), "rel").is_ok());
        assert!(reject_symlink(&root.join("inner"), "rel").is_ok());
        // paths not below the base have no chain to check (harness semantics)
        assert!(first_symlink_below(&root, Path::new("/etc/hosts")).is_none());
        #[cfg(unix)]
        {
            assert_eq!(
                first_symlink_below(&root, &root.join("dirlink/secret/leak.txt")).as_deref(),
                Some(root.join("dirlink").as_path())
            );
            assert!(reject_symlink_chain(&root, &root.join("dirlink/x"), "rel").is_err());
            assert!(reject_symlink(&root.join("filelink"), "rel").is_err());
        }
        fs::remove_dir_all(&wd).unwrap();
    }
}
