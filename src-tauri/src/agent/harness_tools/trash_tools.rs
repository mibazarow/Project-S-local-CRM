// Real macOS Trash tools (Q.md computer-control slice): trash_paths moves
// files/folders into ~/.Trash (name collisions get a numbered name, the
// original path is recorded in a JSON ledger under Settings/), and
// restore_trashed moves items back by ledger id or trashed name. Both are
// destructive/mutating and always confirm. Protected roots (/, ~, /System,
// /Applications, …), the working dir itself and anything already inside the
// Trash are refused. Cross-device moves fall back to copy+remove. The trash
// dir and ledger path are injectable (TrashEnv) so tests use temp dirs and
// never touch the real ~/.Trash.
use super::{canonicalish, lenient_existing, resolve_path};
use crate::agent::project_tools::require_root;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::path_policy::home_dir;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const LEDGER_FILE: &str = "trash-ledger.json";

#[derive(Debug, Serialize, Deserialize)]
struct LedgerEntry {
    id: String,
    original: String,
    name: String,
    at: String,
    size: Option<u64>,
    is_dir: bool,
}

pub(crate) struct TrashEnv<'a> {
    pub trash_dir: &'a Path,
    pub ledger_path: &'a Path,
    pub wd: &'a Path,
}

fn default_trash_dir() -> PathBuf {
    home_dir().join(".Trash")
}

fn default_ledger(wd: &Path) -> PathBuf {
    crate::paths::settings_dir(wd).join(LEDGER_FILE)
}

// Conservative guard: the confirmation is the real gate, this only stops
// the obviously catastrophic calls. Exact roots are the folders themselves;
// prefix roots refuse everything inside them (system binaries, apps).
fn protected_exact(wd: &Path, trash_dir: &Path) -> Vec<(PathBuf, &'static str)> {
    vec![
        (PathBuf::from("/"), "/"),
        (canonicalish(&home_dir()), "~"),
        (canonicalish(wd), "working dir"),
        (PathBuf::from("/Users"), "/Users"),
        (PathBuf::from("/Library"), "/Library"),
        (PathBuf::from("/private"), "/private"),
        (PathBuf::from("/Volumes"), "/Volumes"),
        (canonicalish(trash_dir), "Trash"),
    ]
}

fn protected_prefixes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("/System", "/System"),
        ("/Applications", "/Applications"),
        ("/usr", "/usr"),
        ("/bin", "/bin"),
        ("/sbin", "/sbin"),
        ("/dev", "/dev"),
        ("/etc", "/etc"),
    ]
}

fn forbidden_reason(path: &Path, env: &TrashEnv) -> Option<String> {
    let resolved = canonicalish(path);
    for (root, name) in protected_exact(env.wd, env.trash_dir) {
        if resolved == root {
            return Some(format!(
                "refusing to trash a protected root ({name}): {}",
                path.display()
            ));
        }
    }
    for (root, name) in protected_prefixes() {
        if resolved == Path::new(root) || resolved.starts_with(root) {
            return Some(format!(
                "refusing to trash inside a protected root ({name}): {}",
                path.display()
            ));
        }
    }
    if resolved.starts_with(canonicalish(env.trash_dir)) {
        return Some(format!("path is already inside the Trash: {}", path.display()));
    }
    None
}

fn load_ledger(path: &Path) -> std::result::Result<Vec<LedgerEntry>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("trash ledger {} is corrupt: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read trash ledger {}: {e}", path.display())),
    }
}

fn save_ledger(path: &Path, entries: &[LedgerEntry]) -> std::result::Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(entries).map_err(|e| format!("ledger serialize: {e}"))?;
    fs::write(path, text).map_err(|e| format!("cannot write trash ledger {}: {e}", path.display()))
}

// Finder-style collision naming: "note.txt" -> "note 2.txt" -> "note 3.txt".
fn unique_trash_name(trash_dir: &Path, base: &str) -> String {
    if !trash_dir.join(base).exists() {
        return base.to_string();
    }
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i + 1..]),
        _ => (base, ""),
    };
    for n in 2..10_000 {
        let candidate = if ext.is_empty() {
            format!("{stem} {n}")
        } else {
            format!("{stem} {n}.{ext}")
        };
        if !trash_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{stem} {}", Utc::now().timestamp_millis())
}

fn copy_recursive(src: &Path, dst: &Path) -> std::result::Result<(), std::io::Error> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "symlinks are not copied",
        ));
    }
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn remove_recursive(path: &Path) -> std::result::Result<(), std::io::Error> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

// rename first; on cross-device (or similar) failures fall back to a full
// copy followed by source removal.
fn move_item(src: &Path, dst: &Path) -> std::result::Result<(), std::io::Error> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            copy_recursive(src, dst).map_err(|copy_err| {
                let _ = remove_recursive(dst);
                std::io::Error::new(
                    copy_err.kind(),
                    format!("rename failed ({rename_err}); copy fallback failed ({copy_err})"),
                )
            })?;
            remove_recursive(src).map_err(|rm_err| {
                std::io::Error::new(
                    rm_err.kind(),
                    format!("copied to {} but could not remove source: {rm_err}", dst.display()),
                )
            })
        }
    }
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(m) = fs::symlink_metadata(entry.path()) {
                    if m.is_dir() && !m.file_type().is_symlink() {
                        stack.push(entry.path());
                    } else if m.is_file() {
                        total += m.len();
                    }
                }
            }
        }
    }
    total
}

fn new_entry_id(seq: usize) -> String {
    let now = Utc::now();
    format!(
        "tr-{}-{:06x}-{seq}",
        now.timestamp_millis(),
        now.timestamp_subsec_nanos() % 0x1000000
    )
}

pub(crate) fn trash_paths_impl(
    paths: &[String],
    env: &TrashEnv,
) -> std::result::Result<Vec<Value>, String> {
    fs::create_dir_all(env.trash_dir)
        .map_err(|e| format!("cannot create trash dir {}: {e}", env.trash_dir.display()))?;
    let mut ledger = load_ledger(env.ledger_path)?;
    let mut results: Vec<Value> = Vec::new();
    for raw in paths {
        let resolved = resolve_path(raw, env.wd);
        let path = match lenient_existing(&resolved) {
            Ok(p) => p,
            Err(e) => {
                results.push(json!({ "path": raw, "ok": false, "error": e.to_string() }));
                continue;
            }
        };
        if let Some(reason) = forbidden_reason(&path, env) {
            results.push(json!({ "path": raw, "ok": false, "error": reason }));
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                results.push(json!({ "path": raw, "ok": false, "error": format!("{path:?}: {e}") }));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            results.push(json!({
                "path": raw,
                "ok": false,
                "error": "symlinks are not moved to the Trash; remove them with fs_delete"
            }));
            continue;
        }
        let base = match path.file_name().and_then(|s| s.to_str()) {
            Some(b) => b.to_string(),
            None => {
                results.push(json!({ "path": raw, "ok": false, "error": "cannot name the path" }));
                continue;
            }
        };
        let name = unique_trash_name(env.trash_dir, &base);
        let dst = env.trash_dir.join(&name);
        if let Err(e) = move_item(&path, &dst) {
            results.push(json!({ "path": raw, "ok": false, "error": format!("move failed: {e}") }));
            continue;
        }
        let entry = LedgerEntry {
            id: new_entry_id(ledger.len()),
            original: path.display().to_string(),
            name,
            at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            size: Some(if meta.is_dir() { dir_size(&dst) } else { meta.len() }),
            is_dir: meta.is_dir(),
        };
        results.push(json!({
            "path": raw,
            "ok": true,
            "trashed": dst.display().to_string(),
            "id": entry.id,
        }));
        ledger.push(entry);
        save_ledger(env.ledger_path, &ledger)?;
    }
    Ok(results)
}

// Restore semantics: the item moves from the Trash back to its recorded
// original path; if the original path is occupied the item STAYS in the
// Trash and the per-item result reports an error (no overwrite, no silent
// suffix). Unknown ids/names and items missing from the Trash are errors.
pub(crate) fn restore_trashed_impl(
    items: &[String],
    env: &TrashEnv,
) -> std::result::Result<Vec<Value>, String> {
    let mut ledger = load_ledger(env.ledger_path)?;
    let mut results: Vec<Value> = Vec::new();
    for item in items {
        let idx = ledger.iter().position(|e| e.id == *item || e.name == *item);
        let Some(i) = idx else {
            results.push(json!({ "item": item, "ok": false, "error": "not found in the trash ledger" }));
            continue;
        };
        let entry = &ledger[i];
        let src = env.trash_dir.join(&entry.name);
        if fs::symlink_metadata(&src).is_err() {
            results.push(json!({
                "item": item,
                "ok": false,
                "error": format!("{} is missing from the Trash", src.display())
            }));
            continue;
        }
        let original = PathBuf::from(&entry.original);
        if fs::symlink_metadata(&original).is_ok() {
            results.push(json!({
                "item": item,
                "ok": false,
                "error": format!("original path exists: {}; move or rename it first", original.display())
            }));
            continue;
        }
        if let Some(parent) = original.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                results.push(json!({ "item": item, "ok": false, "error": format!("cannot create {}: {e}", parent.display()) }));
                continue;
            }
        }
        if let Err(e) = move_item(&src, &original) {
            results.push(json!({ "item": item, "ok": false, "error": format!("move failed: {e}") }));
            continue;
        }
        let entry = ledger.remove(i);
        save_ledger(env.ledger_path, &ledger)?;
        results.push(json!({
            "item": item,
            "ok": true,
            "id": entry.id,
            "restored": original.display().to_string(),
        }));
    }
    Ok(results)
}

pub struct TrashPathsTool;

impl Tool for TrashPathsTool {
    fn name(&self) -> &str {
        "trash_paths"
    }
    fn description(&self) -> &str {
        "Move files/folders to the real macOS Trash (~/.Trash): collisions get a numbered name (note.txt -> note 2.txt), originals are recorded in Settings/trash-ledger.json for restore_trashed. Refuses missing paths, symlinks, the working dir, protected roots (/, ~, /Users, /Library, /private, /Volumes and anything under /System, /Applications, /usr, /bin, /sbin, /dev, /etc) and Trash contents. Cross-device falls back to copy+remove. Confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1 }
            },
            "required": ["paths"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("удаление файлов в Корзину macOS".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_root(ctx)?;
        let paths: Vec<String> = args
            .get("paths")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        if paths.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "paths must be a non-empty array of strings"));
        }
        let trash_dir = default_trash_dir();
        let ledger = default_ledger(&wd);
        let env = TrashEnv {
            trash_dir: &trash_dir,
            ledger_path: &ledger,
            wd: &wd,
        };
        Ok(match trash_paths_impl(&paths, &env) {
            Ok(results) => {
                let done = results.iter().filter(|r| r["ok"] == json!(true)).count();
                ToolResult::ok(json!({
                    "results": results,
                    "trashed": done,
                    "failed": results.len() - done,
                }))
                .capped(self.max_result_len())
            }
            Err(m) => ToolResult::err(ToolErrorCode::ExecutionError, m),
        })
    }
}

pub struct RestoreTrashedTool;

impl Tool for RestoreTrashedTool {
    fn name(&self) -> &str {
        "restore_trashed"
    }
    fn description(&self) -> &str {
        "Restore items from the macOS Trash to their recorded original paths via Settings/trash-ledger.json (ledger ids or trashed names). If the original path is occupied the item stays in the Trash with a per-item error (nothing overwritten); missing items and unknown ids are reported per item. Confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ids": { "type": "array", "items": { "type": "string" } },
                "names": { "type": "array", "items": { "type": "string" } }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("восстановление файлов из Корзины macOS".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_root(ctx)?;
        let collect = |key: &str| -> Vec<String> {
            args.get(key)
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default()
        };
        let mut items = collect("ids");
        items.extend(collect("names"));
        if items.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "ids or names must be a non-empty array of strings"));
        }
        let trash_dir = default_trash_dir();
        let ledger = default_ledger(&wd);
        let env = TrashEnv {
            trash_dir: &trash_dir,
            ledger_path: &ledger,
            wd: &wd,
        };
        Ok(match restore_trashed_impl(&items, &env) {
            Ok(results) => {
                let done = results.iter().filter(|r| r["ok"] == json!(true)).count();
                ToolResult::ok(json!({
                    "results": results,
                    "restored": done,
                    "failed": results.len() - done,
                }))
                .capped(self.max_result_len())
            }
            Err(m) => ToolResult::err(ToolErrorCode::ExecutionError, m),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;

    struct TestEnv {
        base: PathBuf,
        wd: PathBuf,
        trash: PathBuf,
        ledger: PathBuf,
    }

    fn test_env(tag: &str) -> TestEnv {
        let base = temp_test_dir(tag);
        let wd = base.join("wd");
        let trash = base.join("trash");
        let ledger = base.join("Settings").join(LEDGER_FILE);
        fs::create_dir_all(&wd).unwrap();
        fs::create_dir_all(&trash).unwrap();
        TestEnv { base, wd, trash, ledger }
    }

    fn env_of<'a>(t: &'a TestEnv) -> TrashEnv<'a> {
        TrashEnv {
            trash_dir: &t.trash,
            ledger_path: &t.ledger,
            wd: &t.wd,
        }
    }

    fn ledger_entries(path: &Path) -> Vec<LedgerEntry> {
        load_ledger(path).unwrap()
    }

    #[test]
    fn trash_and_restore_round_trip_file() {
        let t = test_env("trash-rt");
        let file = t.wd.join("a.txt");
        fs::write(&file, "hello").unwrap();
        let env = env_of(&t);

        let results = trash_paths_impl(&[file.display().to_string()], &env).unwrap();
        assert_eq!(results[0]["ok"], json!(true), "{:?}", results[0]);
        assert!(!file.exists());
        assert_eq!(fs::read(t.trash.join("a.txt")).unwrap(), b"hello");

        let entries = ledger_entries(&t.ledger);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].original, file.display().to_string());
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].size, Some(5));
        assert!(!entries[0].is_dir);

        let id = entries[0].id.clone();
        let results = restore_trashed_impl(&[id], &env).unwrap();
        assert_eq!(results[0]["ok"], json!(true), "{:?}", results[0]);
        assert_eq!(fs::read(&file).unwrap(), b"hello");
        assert!(!t.trash.join("a.txt").exists());
        assert!(ledger_entries(&t.ledger).is_empty());
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn collision_gets_numbered_name_and_dir_round_trip_works() {
        let t = test_env("trash-collide");
        fs::create_dir_all(t.wd.join("x")).unwrap();
        fs::create_dir_all(t.wd.join("y/sub")).unwrap();
        fs::write(t.wd.join("x/note.txt"), "one").unwrap();
        fs::write(t.wd.join("y/note.txt"), "two").unwrap();
        fs::write(t.wd.join("y/sub/deep.md"), "deep").unwrap();
        let env = env_of(&t);

        let results = trash_paths_impl(
            &[
                t.wd.join("x/note.txt").display().to_string(),
                t.wd.join("y/note.txt").display().to_string(),
                t.wd.join("y").display().to_string(),
            ],
            &env,
        )
        .unwrap();
        assert!(results.iter().all(|r| r["ok"] == json!(true)), "{results:?}");
        assert_eq!(fs::read(t.trash.join("note.txt")).unwrap(), b"one");
        assert_eq!(fs::read(t.trash.join("note 2.txt")).unwrap(), b"two");
        assert_eq!(fs::read(t.trash.join("y/sub/deep.md")).unwrap(), b"deep");
        assert!(!t.wd.join("y").exists());

        let entries = ledger_entries(&t.ledger);
        assert_eq!(entries.len(), 3);
        let dir_entry = entries.iter().find(|e| e.is_dir).unwrap();
        // y/note.txt was already trashed before y itself, so only deep.md remains
        assert_eq!(dir_entry.size, Some(4));

        // restore the folder by its trashed name
        let results = restore_trashed_impl(&["y".to_string()], &env).unwrap();
        assert_eq!(results[0]["ok"], json!(true), "{:?}", results[0]);
        assert_eq!(fs::read(t.wd.join("y/sub/deep.md")).unwrap(), b"deep");
        assert_eq!(ledger_entries(&t.ledger).len(), 2);
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn restore_reports_missing_item_and_original_conflict() {
        let t = test_env("trash-errors");
        let a = t.wd.join("a.txt");
        let b = t.wd.join("b.txt");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let env = env_of(&t);
        trash_paths_impl(
            &[a.display().to_string(), b.display().to_string()],
            &env,
        )
        .unwrap();

        fs::remove_file(t.trash.join("a.txt")).unwrap();
        fs::write(&b, "new occupant").unwrap();
        let results = restore_trashed_impl(
            &["a.txt".to_string(), "b.txt".to_string(), "ghost.txt".to_string()],
            &env,
        )
        .unwrap();
        assert!(results[0]["error"]
            .as_str()
            .unwrap()
            .contains("missing from the Trash"));
        assert!(results[1]["error"]
            .as_str()
            .unwrap()
            .contains("original path exists"));
        assert!(results[2]["error"]
            .as_str()
            .unwrap()
            .contains("not found in the trash ledger"));
        assert_eq!(fs::read(t.trash.join("b.txt")).unwrap(), b"b");
        assert_eq!(fs::read(&b).unwrap(), b"new occupant");
        assert_eq!(ledger_entries(&t.ledger).len(), 2);
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn dangerous_paths_are_rejected_and_nothing_moves() {
        let t = test_env("trash-danger");
        let env = env_of(&t);
        let home = home_dir().display().to_string();
        let results = trash_paths_impl(
            &[
                "/".to_string(),
                home,
                t.wd.display().to_string(),
                t.trash.display().to_string(),
                "/System/Library".to_string(),
                "/Applications".to_string(),
                "/nonexistent-xyz-42".to_string(),
            ],
            &env,
        )
        .unwrap();
        assert!(results.iter().all(|r| r["ok"] == json!(false)), "{results:?}");
        assert!(results[0]["error"].as_str().unwrap().contains("protected root"));
        assert!(results[1]["error"].as_str().unwrap().contains("protected root"));
        assert!(results[2]["error"].as_str().unwrap().contains("working dir"));
        assert!(results[3]["error"].as_str().unwrap().contains("protected root"));
        assert!(results[4]["error"].as_str().unwrap().contains("protected root"));
        assert!(results[5]["error"].as_str().unwrap().contains("protected root"));
        assert!(results[6]["error"].as_str().unwrap().contains("not found"));
        assert!(Path::new("/System/Library").exists());
        assert!(Path::new("/Applications").exists());
        assert!(ledger_entries(&t.ledger).is_empty());
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn ledger_persists_across_calls_and_relative_paths_resolve() {
        let t = test_env("trash-ledger");
        fs::write(t.wd.join("one.txt"), "1").unwrap();
        fs::write(t.wd.join("two.txt"), "22").unwrap();
        let env = env_of(&t);
        trash_paths_impl(&["one.txt".to_string()], &env).unwrap();
        trash_paths_impl(&["two.txt".to_string()], &env).unwrap();
        let entries = ledger_entries(&t.ledger);
        assert_eq!(entries.len(), 2);
        assert!(t.trash.join("one.txt").exists());
        assert!(t.trash.join("two.txt").exists());
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn collision_naming_splits_extension_after_first_dot_only_for_hidden() {
        assert_eq!(unique_trash_name(Path::new("/definitely-missing-dir-xyz"), "a.txt"), "a.txt");
        let t = test_env("trash-naming");
        fs::write(t.trash.join("note.txt"), "").unwrap();
        assert_eq!(unique_trash_name(&t.trash, "note.txt"), "note 2.txt");
        fs::write(t.trash.join("note 2.txt"), "").unwrap();
        assert_eq!(unique_trash_name(&t.trash, "note.txt"), "note 3.txt");
        fs::write(t.trash.join(".hidden"), "").unwrap();
        assert_eq!(unique_trash_name(&t.trash, ".hidden"), ".hidden 2");
        fs::remove_dir_all(&t.base).unwrap();
    }

    #[test]
    fn tool_policies_are_check() {
        assert_eq!(TrashPathsTool.class(), ToolClass::Mutating);
        assert_eq!(TrashPathsTool.policy(), PermissionPolicy::Check);
        assert!(TrashPathsTool
            .danger_reason(&json!({ "paths": ["x"] }), &ToolCtx::default())
            .is_some());
        assert_eq!(RestoreTrashedTool.class(), ToolClass::Mutating);
        assert_eq!(RestoreTrashedTool.policy(), PermissionPolicy::Check);
        assert!(RestoreTrashedTool
            .danger_reason(&json!({ "ids": ["x"] }), &ToolCtx::default())
            .is_some());
    }
}
