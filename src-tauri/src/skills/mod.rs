// Skills subsystem core (SPEC 6.2): discovery/parse of
// Settings/skills/{builtin,user}/<name>/SKILL.md, the on-demand `skill`
// tool, the short name+description list injected into system prompts, and
// the management commands (list, enable/disable, import folder/.zip/git,
// builtin set install, create). The skills UI is the next slice.
#![allow(dead_code)] // env_from_settings/scheduled_tasks are parsed now, consumed by later slices

use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::paths;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_SCRIPT_TIMEOUT_SEC: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillKind {
    Instruction,
    Script { timeout_sec: u64 },
    Extension,
}

impl SkillKind {
    pub fn label(&self) -> &'static str {
        match self {
            SkillKind::Instruction => "instruction",
            SkillKind::Script { .. } => "script",
            SkillKind::Extension => "extension",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub kind: SkillKind,
    pub permissions: Option<Value>,
    pub env_from_settings: Option<Vec<String>>,
    pub scheduled_tasks: Option<Value>,
    // The SKILL.md file itself.
    pub path: PathBuf,
    pub builtin: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct SkillError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    // Winners sorted by name. A user skill shadows a builtin of the same
    // name and the name is reported in `conflicts` (SPEC 6.2 "место
    // конфликта со встроенными").
    pub skills: Vec<SkillManifest>,
    pub conflicts: Vec<String>,
    // Invalid skills are skipped with a recorded error, never a crash.
    pub errors: Vec<SkillError>,
}

// Splits SKILL.md into (parsed frontmatter, markdown body). The frontmatter
// must open with a "---" line; the body starts after the closing "---".
// No fences -> (None, whole text).
fn split_frontmatter(text: &str) -> (Option<Value>, &str) {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end() != "---" {
        return (None, text);
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim_end() == "---" {
            let fm = &text[first.len()..offset];
            return (parse_frontmatter(fm), &text[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

// Minimal YAML-subset parser for the frontmatter: mappings, block and flow
// sequences, plain/quoted/folded (>) and literal (|) scalars. Enough for
// the SPEC 6.2 manifest keys; no heavy YAML crate is pulled in for this.
fn parse_frontmatter(src: &str) -> Option<Value> {
    let mut lines: Vec<(String, usize)> = Vec::new();
    for raw in src.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let content = raw.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        lines.push((content.to_string(), indent));
    }
    if lines.is_empty() {
        return None;
    }
    let base = lines[0].1;
    let mut pos = 0;
    parse_node(&mut lines, &mut pos, base)
}

fn parse_node(lines: &mut Vec<(String, usize)>, pos: &mut usize, min_indent: usize) -> Option<Value> {
    let (content, indent) = lines.get(*pos)?.clone();
    if indent < min_indent {
        return None;
    }
    if content == "-" || content.starts_with("- ") {
        parse_seq(lines, pos, indent)
    } else {
        parse_map(lines, pos, indent)
    }
}

fn parse_map(lines: &mut Vec<(String, usize)>, pos: &mut usize, indent: usize) -> Option<Value> {
    let mut map = Map::new();
    while let Some((content, cur)) = lines.get(*pos).cloned() {
        if cur != indent || content == "-" || content.starts_with("- ") {
            break;
        }
        let (key, rest) = split_key(&content)?;
        *pos += 1;
        let value = parse_value(lines, pos, indent, rest);
        map.insert(key.to_string(), value);
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

fn parse_seq(lines: &mut Vec<(String, usize)>, pos: &mut usize, indent: usize) -> Option<Value> {
    let mut items = Vec::new();
    while let Some((content, cur)) = lines.get(*pos).cloned() {
        if cur != indent || !(content == "-" || content.starts_with("- ")) {
            break;
        }
        let item = content[1..].trim_start().to_string();
        if item.is_empty() {
            *pos += 1;
            items.push(parse_node(lines, pos, indent + 1).unwrap_or(Value::Null));
        } else if split_key(&item).is_some() {
            // Mapping item: re-anchor the dash line as a map entry one level
            // deeper so its continuation lines join the same object.
            lines[*pos] = (item, indent + 2);
            items.push(parse_map(lines, pos, indent + 2).unwrap_or(Value::Null));
        } else {
            *pos += 1;
            items.push(parse_value(lines, pos, indent, &item));
        }
    }
    if items.is_empty() {
        None
    } else {
        Some(Value::Array(items))
    }
}

fn parse_value(lines: &mut Vec<(String, usize)>, pos: &mut usize, indent: usize, rest: &str) -> Value {
    if rest.is_empty() {
        return parse_node(lines, pos, indent + 1).unwrap_or(Value::Null);
    }
    if rest.starts_with('>') || rest.starts_with('|') {
        return parse_block_scalar(lines, pos, indent, rest.starts_with('|'));
    }
    if rest.starts_with('[') && rest.ends_with(']') {
        return parse_flow_seq(rest);
    }
    parse_scalar(rest)
}

fn parse_block_scalar(lines: &mut [(String, usize)], pos: &mut usize, indent: usize, literal: bool) -> Value {
    let mut parts: Vec<String> = Vec::new();
    while let Some((content, cur)) = lines.get(*pos).cloned() {
        if cur <= indent {
            break;
        }
        parts.push(content);
        *pos += 1;
    }
    Value::String(parts.join(if literal { "\n" } else { " " }))
}

fn parse_flow_seq(rest: &str) -> Value {
    let inner = rest[1..rest.len() - 1].trim();
    if inner.is_empty() {
        return Value::Array(Vec::new());
    }
    Value::Array(inner.split(',').map(|s| parse_scalar(s.trim())).collect())
}

fn parse_scalar(s: &str) -> Value {
    let s = s.trim();
    let b = s.as_bytes();
    if s.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        return Value::String(s[1..s.len() - 1].to_string());
    }
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" | "~" => Value::Null,
        _ => {
            if let Ok(i) = s.parse::<i64>() {
                return Value::from(i);
            }
            if let Ok(f) = s.parse::<f64>() {
                return Value::from(f);
            }
            Value::String(s.to_string())
        }
    }
}

fn split_key(content: &str) -> Option<(&str, &str)> {
    if let Some(idx) = content.find(": ") {
        Some((content[..idx].trim(), content[idx + 2..].trim()))
    } else {
        content.strip_suffix(':').map(|key| (key.trim(), ""))
    }
}

fn manifest_from_frontmatter(
    fm: &Value,
    folder: &str,
    path: PathBuf,
    builtin: bool,
    disabled: &HashSet<String>,
) -> std::result::Result<SkillManifest, String> {
    let name = fm
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("frontmatter needs a non-empty \"name\"")?;
    // The folder name is the on-disk identity used for enable/disable and
    // the skill tool, so a mismatching manifest name would desync them.
    if name != folder {
        return Err(format!(
            "frontmatter name \"{name}\" does not match folder \"{folder}\""
        ));
    }
    let description = fm
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("frontmatter needs a non-empty \"description\"")?;
    let kind = match fm.get("type").and_then(Value::as_str).unwrap_or("instruction") {
        "instruction" => SkillKind::Instruction,
        "script" => SkillKind::Script {
            timeout_sec: fm
                .get("timeout_sec")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_SEC),
        },
        "extension" => SkillKind::Extension,
        other => {
            return Err(format!(
                "unknown skill type \"{other}\" (instruction|script|extension)"
            ))
        }
    };
    let env_from_settings = match fm.get("env_from_settings") {
        None => None,
        Some(v) => Some(
            v.as_array()
                .ok_or("env_from_settings must be a list of strings")?
                .iter()
                .map(|e| {
                    e.as_str()
                        .map(str::to_string)
                        .ok_or("env_from_settings must be a list of strings")
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ),
    };
    Ok(SkillManifest {
        name: name.to_string(),
        description: description.to_string(),
        kind,
        permissions: fm.get("permissions").cloned(),
        env_from_settings,
        scheduled_tasks: fm.get("scheduled_tasks").cloned(),
        path,
        builtin,
        enabled: !disabled.contains(name),
    })
}

// Enable/disable state lives in Settings/skills/enabled.json, plaintext
// { "disabled": ["name", ...] }. Chosen over an app.json field so the
// state sits next to the skills it controls, survives hand edits, and is
// covered by the skills watcher. A missing or malformed file means
// "everything enabled".
fn disabled_names(wd: &Path) -> HashSet<String> {
    fs::read_to_string(paths::skills_enabled_file(wd))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("disabled").and_then(Value::as_array).cloned())
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// Re-reads the skills tree from disk on every call, so hand edits are
// picked up at the next use; the watcher only needs to notify, not reload.
pub fn discover(wd: &Path) -> SkillSet {
    let disabled = disabled_names(wd);
    let mut set = SkillSet::default();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for (dir, builtin) in [
        (paths::skills_builtin_dir(wd), true),
        (paths::skills_user_dir(wd), false),
    ] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.path().join(paths::SKILL_FILE);
            if !file.is_file() {
                continue;
            }
            let folder = entry.file_name().to_string_lossy().into_owned();
            let text = match fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => {
                    set.errors.push(SkillError {
                        path: file,
                        message: format!("SKILL.md unreadable: {e}"),
                    });
                    continue;
                }
            };
            let Some(fm) = split_frontmatter(&text).0 else {
                set.errors.push(SkillError {
                    path: file,
                    message: "frontmatter is missing or malformed (--- fences)".into(),
                });
                continue;
            };
            match manifest_from_frontmatter(&fm, &folder, file.clone(), builtin, &disabled) {
                Ok(manifest) => match index.get(&manifest.name) {
                    Some(&i) => {
                        // builtin is scanned first: the user skill shadows it
                        // and the name surfaces as a conflict; the shadowed
                        // builtin is reported as an error row so the UI list
                        // shows where the conflict sits.
                        set.conflicts.push(manifest.name.clone());
                        set.errors.push(SkillError {
                            path: set.skills[i].path.clone(),
                            message: format!("conflicts with user skill \"{}\"", manifest.name),
                        });
                        set.skills[i] = manifest;
                    }
                    None => {
                        index.insert(manifest.name.clone(), set.skills.len());
                        set.skills.push(manifest);
                    }
                },
                Err(message) => set.errors.push(SkillError { path: file, message }),
            }
        }
    }
    set.skills.sort_by(|a, b| a.name.cmp(&b.name));
    set
}

// The only skills content that reaches the system prompt: one
// name+description line per enabled skill (SPEC 6.2 "только короткий список").
pub fn short_list(wd: &Path) -> Vec<(String, String)> {
    discover(wd)
        .skills
        .into_iter()
        .filter(|s| s.enabled)
        .map(|s| (s.name, s.description))
        .collect()
}

// Skill names are matched leniently: "grill.me", "Grill Me", "grill_me"
// all resolve to the canonical "grill-me" folder/manifest name.
pub fn normalize_skill_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c == '.' || c == '_' || c.is_whitespace() {
            out.push('-');
        } else {
            out.extend(c.to_lowercase());
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

// Close-name hints for a miss: prefix/contains scoring against normalized
// names, best 3.
fn suggest_skill_names(names: &[String], norm: &str) -> Vec<String> {
    if norm.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u8, &String)> = Vec::new();
    for name in names {
        let n = normalize_skill_name(name);
        if n == norm {
            continue;
        }
        let score = if n.starts_with(norm) || norm.starts_with(&n) {
            1
        } else if n.contains(norm) || norm.contains(&n) {
            2
        } else {
            continue;
        };
        scored.push((score, name));
    }
    scored.sort_by_key(|(score, name)| (*score, name.as_str()));
    scored.into_iter().take(3).map(|(_, name)| name.clone()).collect()
}

pub fn find(wd: &Path, name: &str) -> Result<SkillManifest> {
    let set = discover(wd);
    let norm = normalize_skill_name(name);
    match set.skills.iter().find(|s| normalize_skill_name(&s.name) == norm) {
        None => {
            let names: Vec<String> = set.skills.iter().map(|s| s.name.clone()).collect();
            let hints = suggest_skill_names(&names, &norm);
            let hint = if hints.is_empty() {
                String::new()
            } else {
                format!(" (closest: {})", hints.join(", "))
            };
            Err(AppError::Invalid(format!("skill not found: {name}{hint}")))
        }
        Some(s) if !s.enabled => Err(AppError::Invalid(format!("skill disabled: {name}"))),
        Some(s) => Ok(s.clone()),
    }
}

// Full SKILL.md body without the frontmatter. Script skills get a one-line
// entrypoint note (executing them is a later slice).
pub fn body(skill: &SkillManifest) -> Result<String> {
    let text = fs::read_to_string(&skill.path)?;
    let (fm, body) = split_frontmatter(&text);
    let body = body.trim().to_string();
    if let SkillKind::Script { timeout_sec } = skill.kind {
        let entry = fm
            .as_ref()
            .and_then(|fm| fm.get("entry").and_then(Value::as_str))
            .or_else(|| {
                fm.as_ref()
                    .and_then(|fm| fm.get("scripts").and_then(Value::as_array))
                    .and_then(|scripts| scripts.first())
                    .and_then(|s| s.as_str().or_else(|| s.get("name").and_then(Value::as_str)))
            })
            .unwrap_or("scripts/");
        return Ok(format!(
            "> Skill script: entrypoint — {entry}, timeout {timeout_sec}s. Below is the full instruction text.\n\n{body}"
        ));
    }
    Ok(body)
}

pub fn load_full(wd: &Path, name: &str) -> Result<String> {
    body(&find(wd, name)?)
}

// ---------------------------------------------------------------------------
// Management (SPEC 6.2): the backend behind the skills screen — list with
// errors/conflicts, enable/disable, import (folder/.zip/git), builtin set
// install, user skill creation. lib.rs wraps these as tauri commands.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub kind: String,
    pub builtin: bool,
    pub enabled: bool,
    pub path: String,
    // Set for invalid skills and for builtins shadowed by a user skill.
    pub error: Option<String>,
}

fn skill_info(manifest: &SkillManifest) -> SkillInfo {
    SkillInfo {
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        kind: manifest.kind.label().to_string(),
        builtin: manifest.builtin,
        enabled: manifest.enabled,
        path: manifest.path.display().to_string(),
        error: None,
    }
}

pub fn list(wd: &Path) -> Vec<SkillInfo> {
    let set = discover(wd);
    let builtin_root = paths::skills_builtin_dir(wd);
    let mut infos: Vec<SkillInfo> = set.skills.iter().map(skill_info).collect();
    for err in &set.errors {
        infos.push(SkillInfo {
            name: err
                .path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            description: String::new(),
            kind: String::new(),
            builtin: err.path.starts_with(&builtin_root),
            enabled: false,
            path: err.path.display().to_string(),
            error: Some(err.message.clone()),
        });
    }
    infos.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.error.cmp(&b.error)));
    infos
}

pub fn set_enabled(wd: &Path, name: &str, enabled: bool) -> Result<()> {
    if !discover(wd).skills.iter().any(|s| s.name == name) {
        return Err(AppError::Invalid(format!("skill not found: {name}")));
    }
    let mut disabled: BTreeSet<String> = disabled_names(wd).into_iter().collect();
    if enabled {
        disabled.remove(name);
    } else {
        disabled.insert(name.to_string());
    }
    let value = json!({ "disabled": disabled.into_iter().collect::<Vec<_>>() });
    fs::create_dir_all(paths::skills_dir(wd))?;
    fs::write(paths::skills_enabled_file(wd), serde_json::to_string_pretty(&value)?)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub name: String,
    pub ok: bool,
    pub error: Option<String>,
}

fn zip_err(e: zip::result::ZipError) -> AppError {
    AppError::Io(format!("zip: {e}"))
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
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

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

// A source holds either one skill (SKILL.md at its root) or several skills
// in its direct subfolders; hidden folders (.git of a clone) are skipped.
fn find_skill_dirs(root: &Path) -> Vec<PathBuf> {
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

fn validate_skill_dir(dir: &Path) -> std::result::Result<SkillManifest, String> {
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

// Extract a .zip into `dest`, rejecting unsafe entries: `enclosed_name()`
// drops anything escaping via parent dirs (returns None); raw absolute
// paths are rejected outright instead of being silently stripped.
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_err)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_err)?;
        let raw = entry.name().to_string();
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err(AppError::Invalid(format!("zip entry has an absolute path: {raw:?}")));
        }
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| AppError::Invalid(format!("zip entry has an unsafe path: {raw:?}")))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out_file = fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut out_file)?;
        }
    }
    Ok(())
}

// Git import shells out to the system git CLI (no git crate is pulled in);
// TMPDIR/TEMP/TMP point at Harness/tmp/ like the bash tool (SPEC 5.2).
// `exe` is a seam so the missing-git error path is testable.
fn run_git(wd: &Path, exe: &str, args: &[&str]) -> Result<()> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let out = Command::new(exe)
        .args(args)
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => AppError::Invalid(format!(
                "git executable \"{exe}\" not found: import from git needs the system git CLI"
            )),
            _ => AppError::Io(format!("git spawn: {e}")),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(AppError::Invalid(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}

// Installs each candidate skill folder into Settings/skills/user/. A name
// clashing with a builtin is rejected (SPEC 6.2 "место конфликта со
// встроенными"); an existing user skill is only replaced with `overwrite`.
// When every candidate fails the whole import surfaces as one error.
fn import_candidates(wd: &Path, dirs: &[PathBuf], overwrite: bool) -> Result<Vec<ImportResult>> {
    if dirs.is_empty() {
        return Err(AppError::Invalid(
            "no SKILL.md found at the top level of the import source".into(),
        ));
    }
    let user = paths::skills_user_dir(wd);
    let builtin = paths::skills_builtin_dir(wd);
    let mut results = Vec::new();
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let outcome: std::result::Result<(), String> = (|| {
            validate_skill_dir(dir)?;
            if builtin.join(&name).join(paths::SKILL_FILE).is_file() {
                return Err(format!("\"{name}\" conflicts with a builtin skill"));
            }
            let dest = user.join(&name);
            if dest.exists() {
                if !overwrite {
                    return Err(format!("skill \"{name}\" already exists (pass overwrite to replace)"));
                }
                if same_path(dir, &dest) {
                    return Err(format!("\"{name}\" source is the install location itself"));
                }
                fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
            }
            fs::create_dir_all(&user).map_err(|e| e.to_string())?;
            copy_dir_all(dir, &dest)
                .map_err(|e| e.to_string())
                .and_then(|()| match validate_skill_dir(&dest) {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        let _ = fs::remove_dir_all(&dest);
                        Err(e)
                    }
                })
        })();
        let error = outcome.err().map(|e| e.to_string());
        results.push(ImportResult {
            name,
            ok: error.is_none(),
            error,
        });
    }
    if results.iter().all(|r| !r.ok) {
        let joined = results
            .iter()
            .filter_map(|r| r.error.clone())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(AppError::Invalid(joined));
    }
    Ok(results)
}

pub fn import(wd: &Path, source: &str, target: &str, overwrite: bool) -> Result<Vec<ImportResult>> {
    let target = target.trim();
    if target.is_empty() {
        return Err(AppError::Invalid("import target path is empty".into()));
    }
    // zip/git land in a staging folder under Harness/tmp/, cleaned up after.
    let staging = paths::harness_tmp_dir(wd).join(format!("skill-import-{}", unique_suffix()));
    let prepared: Result<PathBuf> = (|| match source {
        "folder" => {
            let root = PathBuf::from(target);
            if !root.is_dir() {
                return Err(AppError::Invalid(format!("folder not found: {target}")));
            }
            Ok(root)
        }
        "zip" => {
            let file = PathBuf::from(target);
            if !file.is_file() {
                return Err(AppError::Invalid(format!("zip file not found: {target}")));
            }
            fs::create_dir_all(&staging)?;
            extract_zip(&file, &staging)?;
            Ok(staging.clone())
        }
        "git" => {
            if target.starts_with('-') {
                return Err(AppError::Invalid("git url must not start with '-'".into()));
            }
            fs::create_dir_all(&staging)?;
            let repo = staging.join("repo");
            run_git(
                wd,
                "git",
                &["clone", "--depth", "1", "--", target, &repo.display().to_string()],
            )?;
            Ok(repo)
        }
        other => Err(AppError::Invalid(format!(
            "unknown import source: {other} (folder|zip|git)"
        ))),
    })();
    let result = match prepared {
        Ok(root) => import_candidates(wd, &find_skill_dirs(&root), overwrite),
        Err(e) => Err(e),
    };
    let _ = fs::remove_dir_all(&staging);
    result
}

// Builtin skill set (SPEC 6.2 "копируется полный набор скиллов coding-агента
// ... обновляются с приложением"): copies skill folders into
// Settings/skills/builtin/, refreshing existing builtin copies. Source
// resolution order: the repo resources under src-tauri/resources/skills/builtin
// (dev and test builds), then the packaged resources relative to the
// executable, then the standard kilo dev skill folders ~/.config/kilo/skills
// and ~/.kilocode/skills as a last-resort fallback. Never writes into the
// user dir, so user skills are never clobbered.
pub fn install_builtin(wd: &Path) -> Result<Vec<String>> {
    let src = resolve_builtin_source().ok_or_else(|| {
        AppError::Invalid(
            "no builtin skill source found (expected resources/skills/builtin in the repo or packaged resources, or ~/.config/kilo/skills, or ~/.kilocode/skills)".into(),
        )
    })?;
    install_builtin_from(wd, &src)
}

fn resolve_builtin_source() -> Option<PathBuf> {
    // 1. Repo resources (dev/test builds): src-tauri/resources/skills/builtin.
    //    In a packaged app this build-machine path does not exist, so the
    //    check simply falls through.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("skills")
        .join(paths::SKILLS_BUILTIN);
    if repo.is_dir() {
        return Some(repo);
    }
    // 2. Packaged resources relative to the executable.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // macOS app bundle: Contents/MacOS/<exe> sits next to
            // Contents/Resources, where bundle.resources "resources/skills" lands.
            if let Some(contents) = exe_dir.parent() {
                let bundled = contents
                    .join("Resources")
                    .join("resources")
                    .join("skills")
                    .join(paths::SKILLS_BUILTIN);
                if bundled.is_dir() {
                    return Some(bundled);
                }
            }
            // Flat layout: skills/builtin directly next to the executable.
            let cand = exe_dir.join("skills").join(paths::SKILLS_BUILTIN);
            if cand.is_dir() {
                return Some(cand);
            }
        }
    }
    // 3. Dev fallback: the standard kilo skill folders.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home_skill_source(home.as_deref())
}

fn home_skill_source(home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    [".config/kilo/skills", ".kilocode/skills"]
        .iter()
        .map(|sub| home.join(sub))
        .find(|p| p.is_dir())
}

pub fn install_builtin_from(wd: &Path, src: &Path) -> Result<Vec<String>> {
    if !src.is_dir() {
        return Err(AppError::Invalid(format!(
            "builtin skill source not found: {}",
            src.display()
        )));
    }
    let dest_root = paths::skills_builtin_dir(wd);
    fs::create_dir_all(&dest_root)?;
    let mut installed = Vec::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !path.is_dir() || !path.join(paths::SKILL_FILE).is_file() {
            continue;
        }
        let dest = dest_root.join(entry.file_name());
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        copy_dir_all(&path, &dest)?;
        installed.push(name);
    }
    installed.sort();
    Ok(installed)
}

// Startup sync of the bundled builtin set into an EXISTING working dir:
// copies only missing or changed builtin skill folders (SKILL.md content
// differs byte-for-byte), never touches user/, and never deletes builtin
// dirs the bundle no longer ships (e.g. an older bundle's "scrapling").
// Idempotent; the app version is recorded in Settings/skills/.builtin-version.
pub fn sync_builtin(wd: &Path) -> Result<Vec<String>> {
    let src = resolve_builtin_source().ok_or_else(|| {
        AppError::Invalid(
            "no builtin skill source found (expected resources/skills/builtin in the repo or packaged resources, or ~/.config/kilo/skills, or ~/.kilocode/skills)".into(),
        )
    })?;
    sync_builtin_from(wd, &src)
}

fn sync_builtin_from(wd: &Path, src: &Path) -> Result<Vec<String>> {
    if !src.is_dir() {
        return Err(AppError::Invalid(format!(
            "builtin skill source not found: {}",
            src.display()
        )));
    }
    let dest_root = paths::skills_builtin_dir(wd);
    fs::create_dir_all(&dest_root)?;
    let mut synced = Vec::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !path.is_dir() || !path.join(paths::SKILL_FILE).is_file() {
            continue;
        }
        let dest = dest_root.join(&name);
        if builtin_copy_up_to_date(&path, &dest) {
            continue;
        }
        if dest.exists() {
            fs::remove_dir_all(&dest)?;
        }
        copy_dir_all(&path, &dest)?;
        synced.push(name);
    }
    fs::write(
        paths::skills_builtin_version_file(wd),
        format!("{}\n", env!("CARGO_PKG_VERSION")),
    )?;
    synced.sort();
    Ok(synced)
}

// A builtin copy is current when its SKILL.md matches the bundled one
// byte-for-byte; missing, changed or unreadable copies are re-installed.
fn builtin_copy_up_to_date(src: &Path, dest: &Path) -> bool {
    match fs::read(src.join(paths::SKILL_FILE)) {
        Ok(src_bytes) => fs::read(dest.join(paths::SKILL_FILE)).is_ok_and(|dst| dst == src_bytes),
        Err(_) => false,
    }
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// Creates a user skill (SPEC 6.2 "создание скиллов"): writes
// Settings/skills/user/<name>/SKILL.md and returns it once the loader
// confirms it parses. An existing user skill is only replaced with
// `overwrite`; a builtin of the same name is always refused (the user dir
// would shadow it, which the conflict rules already surface as an error).
pub fn create(
    wd: &Path,
    name: &str,
    description: &str,
    kind: Option<&str>,
    body: Option<&str>,
    overwrite: bool,
) -> Result<SkillInfo> {
    if !valid_skill_name(name) {
        return Err(AppError::Invalid(format!(
            "invalid skill name \"{name}\": use lowercase letters, digits and hyphens"
        )));
    }
    let description = description.trim();
    if description.is_empty() || description.contains('\n') {
        return Err(AppError::Invalid("description must be one non-empty line".into()));
    }
    let kind = match kind.unwrap_or("instruction") {
        "instruction" => None,
        k @ ("script" | "extension") => Some(k),
        other => {
            return Err(AppError::Invalid(format!(
                "unknown skill type \"{other}\" (instruction|script|extension)"
            )))
        }
    };
    if paths::skills_builtin_dir(wd).join(name).join(paths::SKILL_FILE).is_file() {
        return Err(AppError::Invalid(format!(
            "\"{name}\" conflicts with a builtin skill"
        )));
    }
    let dest = paths::skills_user_dir(wd).join(name);
    if dest.exists() {
        if !overwrite {
            return Err(AppError::Invalid(format!(
                "skill \"{name}\" already exists (pass overwrite to replace)"
            )));
        }
        fs::remove_dir_all(&dest)?;
    }
    let mut fm = format!("---\nname: {name}\ndescription: {description}\n");
    if let Some(k) = kind {
        fm.push_str(&format!("type: {k}\n"));
    }
    fm.push_str("---\n");
    let body = body.unwrap_or("").trim();
    let text = if body.is_empty() {
        fm
    } else {
        format!("{fm}\n{body}\n")
    };
    fs::create_dir_all(&dest)?;
    fs::write(dest.join(paths::SKILL_FILE), &text)?;
    match discover(wd).skills.iter().find(|s| s.name == name) {
        Some(manifest) => Ok(skill_info(manifest)),
        None => {
            let _ = fs::remove_dir_all(&dest);
            Err(AppError::Invalid("created skill failed validation in the loader".into()))
        }
    }
}

// The on-demand `skill` tool (SPEC 6.2): loads the full SKILL.md text by
// name. Declared permissions are surfaced in the result; enforcing them is
// a later concern, so nothing is blocked here.
pub struct SkillTool;

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Load the full text of an available skill by name. The system prompt lists skills with one-line descriptions; call this before following a skill to read its complete instructions."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn max_result_len(&self) -> usize {
        // Skill bodies are meant to be read whole; keep the cap generous.
        40_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let name = args["name"].as_str().unwrap_or_default();
        let skill = match find(&wd, name) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                let code = if msg.contains("skill disabled") {
                    ToolErrorCode::Hidden
                } else {
                    ToolErrorCode::NotFound
                };
                return Ok(ToolResult::err(code, msg));
            }
        };
        let content = body(&skill)?;
        Ok(
            ToolResult::ok(json!({
                "name": skill.name,
                "kind": skill.kind.label(),
                "builtin": skill.builtin,
                "permissions": skill.permissions,
                "content": content,
            }))
            .capped(self.max_result_len()),
        )
    }
}

// The `create_skill` agent tool: creates a user skill through the same
// `create` logic the UI uses. Mutating but Skip: it only ever writes inside
// Settings/skills/user/. An existing user skill is replaced only with an
// explicit overwrite=true; builtin names are always refused.
pub struct CreateSkillTool;

impl Tool for CreateSkillTool {
    fn name(&self) -> &str {
        "create_skill"
    }
    fn description(&self) -> &str {
        "Create a user skill saved to Settings/skills/user/<name>/SKILL.md; it then appears in the skills list and loads via the skill tool. Name: lowercase letters, digits and hyphens. kind defaults to instruction. Fails if the name exists or clashes with a builtin; pass overwrite=true to replace an existing user skill (builtins are never replaced)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "kind": { "type": "string", "enum": ["instruction", "script", "extension"] },
                "body": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["name", "description"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let name = args["name"].as_str().unwrap_or_default();
        let description = args["description"].as_str().unwrap_or_default();
        let kind = args.get("kind").and_then(Value::as_str);
        let body = args.get("body").and_then(Value::as_str);
        let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
        match create(&wd, name, description, kind, body, overwrite) {
            Ok(info) => Ok(ToolResult::ok(json!({ "skill": info }))),
            Err(e) => {
                let msg = e.to_string();
                let code = if msg.contains("conflicts with a builtin") {
                    ToolErrorCode::PermissionDenied
                } else if msg.contains("invalid skill name")
                    || msg.contains("description")
                    || msg.contains("unknown skill type")
                    || msg.contains("already exists")
                {
                    ToolErrorCode::InvalidArgs
                } else {
                    ToolErrorCode::ExecutionError
                };
                Ok(ToolResult::err(code, msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;

    fn write_skill(root: &Path, folder: &str, content: &str) {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parses_full_and_minimal_frontmatter() {
        let full = "---\nname: weather\ndescription: Live weather\ntype: script\ntimeout_sec: 90\n\
                    permissions: [net, tool]\nenv_from_settings:\n  - OPENROUTER_API_KEY\n\
                    scheduled_tasks:\n  - name: refresh\n    cron: \"0 * * * *\"\n---\n\n# Weather\n\nBody text.\n";
        let (fm, body) = split_frontmatter(full);
        let fm = fm.unwrap();
        assert_eq!(fm["name"], "weather");
        assert_eq!(fm["timeout_sec"], 90);
        assert_eq!(fm["permissions"], json!(["net", "tool"]));
        assert_eq!(fm["env_from_settings"], json!(["OPENROUTER_API_KEY"]));
        assert_eq!(fm["scheduled_tasks"][0]["cron"], "0 * * * *");
        assert_eq!(body.trim(), "# Weather\n\nBody text.");
        let m = manifest_from_frontmatter(&fm, "weather", PathBuf::from("/x"), false, &HashSet::new())
            .unwrap();
        assert_eq!(m.kind, SkillKind::Script { timeout_sec: 90 });
        assert_eq!(m.permissions, Some(json!(["net", "tool"])));
        assert_eq!(
            m.env_from_settings,
            Some(vec!["OPENROUTER_API_KEY".to_string()])
        );
        assert!(m.scheduled_tasks.is_some());

        // Optional keys absent, folded multi-line description (as in real
        // kilo skills): defaults to instruction, no permissions.
        let minimal = "---\nname: simple\ndescription: >\n  Folded description that\n  spans lines.\n---\nBody.\n";
        let (fm, _) = split_frontmatter(minimal);
        let m = manifest_from_frontmatter(&fm.unwrap(), "simple", PathBuf::from("/y"), true, &HashSet::new())
            .unwrap();
        assert_eq!(m.kind, SkillKind::Instruction);
        assert!(m.permissions.is_none());
        assert_eq!(m.description, "Folded description that spans lines.");
        assert!(m.builtin);
    }

    #[test]
    fn discovery_finds_builtin_and_user_skills_sorted() {
        let wd = temp_test_dir("skills-discover");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "beta",
            "---\nname: beta\ndescription: builtin beta\n---\nB body.\n",
        );
        write_skill(
            &paths::skills_user_dir(&wd),
            "alpha",
            "---\nname: alpha\ndescription: user alpha\n---\nA body.\n",
        );
        let set = discover(&wd);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        let names: Vec<&str> = set.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert!(!set.skills[0].builtin);
        assert!(set.skills[1].builtin);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn user_skill_shadows_builtin_and_conflict_is_reported() {
        let wd = temp_test_dir("skills-conflict");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "dup",
            "---\nname: dup\ndescription: builtin copy\n---\nB.\n",
        );
        write_skill(
            &paths::skills_user_dir(&wd),
            "dup",
            "---\nname: dup\ndescription: user copy wins\n---\nU.\n",
        );
        let set = discover(&wd);
        assert_eq!(set.skills.len(), 1);
        assert_eq!(set.skills[0].description, "user copy wins");
        assert!(!set.skills[0].builtin);
        assert_eq!(set.conflicts, vec!["dup".to_string()]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn invalid_skills_are_skipped_with_recorded_errors() {
        let wd = temp_test_dir("skills-invalid");
        let builtin = paths::skills_builtin_dir(&wd);
        write_skill(&builtin, "nofm", "no frontmatter here\n");
        write_skill(&builtin, "mismatch", "---\nname: other\ndescription: x\n---\nB.\n");
        write_skill(&builtin, "badtype", "---\nname: badtype\ndescription: x\ntype: magic\n---\nB.\n");
        write_skill(&builtin, "good", "---\nname: good\ndescription: ok\n---\nB.\n");
        let set = discover(&wd);
        assert_eq!(set.skills.len(), 1);
        assert_eq!(set.skills[0].name, "good");
        assert_eq!(set.errors.len(), 3);
        assert!(set.errors.iter().any(|e| e.message.contains("frontmatter")));
        assert!(set.errors.iter().any(|e| e.message.contains("does not match folder")));
        assert!(set.errors.iter().any(|e| e.message.contains("unknown skill type")));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn disabled_skills_leave_short_list_and_load_full() {
        let wd = temp_test_dir("skills-disabled");
        let user = paths::skills_user_dir(&wd);
        write_skill(&user, "on", "---\nname: on\ndescription: enabled one\n---\nOn body.\n");
        write_skill(&user, "off", "---\nname: off\ndescription: disabled one\n---\nOff body.\n");
        fs::write(paths::skills_enabled_file(&wd), "{ \"disabled\": [\"off\"] }\n").unwrap();
        let list = short_list(&wd);
        assert_eq!(list, vec![("on".to_string(), "enabled one".to_string())]);
        assert_eq!(load_full(&wd, "on").unwrap(), "On body.");
        let err = load_full(&wd, "off").unwrap_err().to_string();
        assert!(err.contains("skill disabled: off"), "{err}");
        let err = load_full(&wd, "ghost").unwrap_err().to_string();
        assert!(err.contains("skill not found: ghost"), "{err}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn script_skill_body_notes_the_entrypoint() {
        let wd = temp_test_dir("skills-script");
        write_skill(
            &paths::skills_user_dir(&wd),
            "runner",
            "---\nname: runner\ndescription: runs stuff\ntype: script\ntimeout_sec: 30\n\
             scripts:\n  - name: go.py\n    description: the one\n---\nRun it.\n",
        );
        let text = load_full(&wd, "runner").unwrap();
        assert!(text.contains("go.py"), "{text}");
        assert!(text.contains("30"), "{text}");
        assert!(text.contains("Run it."), "{text}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_tool_returns_body_and_errors_for_unknown_or_disabled() {
        let wd = temp_test_dir("skills-tool");
        let user = paths::skills_user_dir(&wd);
        write_skill(
            &user,
            "demo",
            "---\nname: demo\ndescription: demo skill\npermissions: [net]\n---\nDemo instructions.\n",
        );
        write_skill(&user, "off", "---\nname: off\ndescription: off skill\n---\nNope.\n");
        fs::write(paths::skills_enabled_file(&wd), "{ \"disabled\": [\"off\"] }").unwrap();
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = SkillTool;

        let r = tool.execute(&json!({ "name": "demo" }), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["name"], "demo");
        assert_eq!(data["kind"], "instruction");
        assert_eq!(data["builtin"], false);
        assert_eq!(data["permissions"], json!(["net"]));
        assert!(data["content"].as_str().unwrap().contains("Demo instructions."));

        let r = tool.execute(&json!({ "name": "ghost" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        let r = tool.execute(&json!({ "name": "off" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "hidden");
        fs::remove_dir_all(&wd).unwrap();
    }

    fn scaffold(tag: &str) -> PathBuf {
        let wd = temp_test_dir(tag);
        for dir in paths::scaffold_dirs(&wd) {
            fs::create_dir_all(dir).unwrap();
        }
        wd
    }

    fn make_zip(path: &Path, entries: &[(&str, &str)]) {
        use zip::write::FileOptions;
        use zip::ZipWriter;
        let file = fs::File::create(path).unwrap();
        let mut w = ZipWriter::new(file);
        let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, content) in entries {
            w.start_file(name, opts).unwrap();
            std::io::Write::write_all(&mut w, content.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn list_reports_skills_with_flags_errors_and_conflicts() {
        let wd = temp_test_dir("skills-list");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "beta",
            "---\nname: beta\ndescription: builtin beta\n---\nB.\n",
        );
        write_skill(
            &paths::skills_user_dir(&wd),
            "alpha",
            "---\nname: alpha\ndescription: user alpha\n---\nA.\n",
        );
        write_skill(&paths::skills_user_dir(&wd), "broken", "no frontmatter\n");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "dup",
            "---\nname: dup\ndescription: builtin dup\n---\nB.\n",
        );
        write_skill(
            &paths::skills_user_dir(&wd),
            "dup",
            "---\nname: dup\ndescription: user dup\n---\nU.\n",
        );
        fs::write(paths::skills_enabled_file(&wd), "{ \"disabled\": [\"beta\"] }").unwrap();
        let infos = list(&wd);
        let alpha = infos.iter().find(|i| i.name == "alpha").unwrap();
        assert!(!alpha.builtin && alpha.enabled && alpha.error.is_none());
        assert_eq!(alpha.kind, "instruction");
        let beta = infos.iter().find(|i| i.name == "beta").unwrap();
        assert!(beta.builtin && !beta.enabled);
        let broken = infos.iter().find(|i| i.name == "broken").unwrap();
        assert!(broken.error.as_deref().unwrap().contains("frontmatter"));
        // the shadowed builtin surfaces as an error row, the user winner as a normal row
        let dup_err = infos
            .iter()
            .find(|i| i.name == "dup" && i.error.is_some())
            .unwrap();
        assert!(dup_err.builtin);
        assert!(dup_err.error.as_deref().unwrap().contains("conflicts with user skill"));
        let dup_ok = infos.iter().find(|i| i.name == "dup" && i.error.is_none()).unwrap();
        assert!(!dup_ok.builtin);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn set_enabled_persists_and_reflects_in_short_list() {
        let wd = temp_test_dir("skills-toggle");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "b",
            "---\nname: b\ndescription: bb\n---\nB.\n",
        );
        write_skill(
            &paths::skills_user_dir(&wd),
            "u",
            "---\nname: u\ndescription: uu\n---\nU.\n",
        );
        set_enabled(&wd, "b", false).unwrap();
        let raw: Value =
            serde_json::from_str(&fs::read_to_string(paths::skills_enabled_file(&wd)).unwrap())
                .unwrap();
        assert_eq!(raw["disabled"], json!(["b"]));
        assert_eq!(short_list(&wd), vec![("u".to_string(), "uu".to_string())]);
        set_enabled(&wd, "b", true).unwrap();
        assert_eq!(short_list(&wd).len(), 2);
        assert!(set_enabled(&wd, "ghost", false).is_err());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn import_folder_installs_skills_and_enforces_conflict_rules() {
        let wd = scaffold("skills-import-folder");
        let src = wd.join("src-skills");
        write_skill(&src, "one", "---\nname: one\ndescription: first\n---\nOne.\n");
        write_skill(
            &src,
            "two",
            "---\nname: two\ndescription: second\ntype: script\ntimeout_sec: 5\n---\nTwo.\n",
        );
        write_skill(&src, "ponytail", "---\nname: ponytail\ndescription: fake\n---\nX.\n");
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "ponytail",
            "---\nname: ponytail\ndescription: real builtin\n---\nB.\n",
        );
        let results = import(&wd, "folder", src.to_str().unwrap(), false).unwrap();
        assert_eq!(results.iter().filter(|r| r.ok).count(), 2);
        let conflict = results.iter().find(|r| r.name == "ponytail").unwrap();
        assert!(!conflict.ok && conflict.error.as_deref().unwrap().contains("builtin"));
        assert!(paths::skills_user_dir(&wd).join("one/SKILL.md").is_file());
        assert_eq!(load_full(&wd, "one").unwrap(), "One.");

        // re-import: every candidate fails -> one combined error
        let err = import(&wd, "folder", src.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");

        // overwrite=true replaces the user copies but never the builtin clash
        fs::write(src.join("one/SKILL.md"), "---\nname: one\ndescription: updated\n---\nOne v2.\n").unwrap();
        let results = import(&wd, "folder", src.to_str().unwrap(), true).unwrap();
        assert!(results.iter().any(|r| r.name == "one" && r.ok));
        assert!(!results.iter().any(|r| r.name == "ponytail" && r.ok));
        assert_eq!(load_full(&wd, "one").unwrap(), "One v2.");

        // a single-skill folder (SKILL.md at its root) installs by folder name
        let solo = wd.join("solo");
        fs::create_dir_all(&solo).unwrap();
        fs::write(solo.join("SKILL.md"), "---\nname: solo\ndescription: single folder\n---\nSolo.\n").unwrap();
        let results = import(&wd, "folder", solo.to_str().unwrap(), false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok);
        assert!(paths::skills_user_dir(&wd).join("solo/SKILL.md").is_file());

        // staging under Harness/tmp is cleaned up
        assert_eq!(fs::read_dir(paths::harness_tmp_dir(&wd)).unwrap().count(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn import_zip_installs_skill_and_rejects_unsafe_entries() {
        let wd = scaffold("skills-import-zip");
        let good = wd.join("good.zip");
        make_zip(
            &good,
            &[
                ("pack/SKILL.md", "---\nname: pack\ndescription: from zip\n---\nZipped.\n"),
                ("pack/assets/data.txt", "payload"),
            ],
        );
        let results = import(&wd, "zip", good.to_str().unwrap(), false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].ok && results[0].name == "pack");
        assert_eq!(load_full(&wd, "pack").unwrap(), "Zipped.");
        assert!(paths::skills_user_dir(&wd).join("pack/assets/data.txt").is_file());

        let evil = wd.join("evil.zip");
        make_zip(
            &evil,
            &[
                ("pack/SKILL.md", "---\nname: pack\ndescription: ok\n---\nOk.\n"),
                ("../evil.txt", "escape"),
            ],
        );
        let err = import(&wd, "zip", evil.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsafe path"), "{err}");
        assert!(!wd.join("evil.txt").exists());
        assert!(!paths::harness_tmp_dir(&wd).join("evil.txt").exists());

        let abs = wd.join("abs.zip");
        make_zip(&abs, &[("/abs/evil.txt", "nope")]);
        let err = import(&wd, "zip", abs.to_str().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute path"), "{err}");
        assert!(!wd.join("abs/evil.txt").exists());

        assert_eq!(fs::read_dir(paths::harness_tmp_dir(&wd)).unwrap().count(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn git_import_reports_clear_errors_without_network() {
        let wd = scaffold("skills-import-git");
        // missing git executable: clear error, no panic
        let err = run_git(&wd, "definitely-not-a-git-xyz", &["--version"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        // cloning a nonexistent local repo fails fast (offline) and surfaces
        // the git error; on a machine without git the spawn error mentions git too
        let err = import(&wd, "git", "/nonexistent/repo-xyz", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("git"), "{err}");
        // option-injection guard: url starting with '-' is rejected pre-spawn
        assert!(import(&wd, "git", "--upload-pack=evil", false).is_err());
        assert_eq!(fs::read_dir(paths::harness_tmp_dir(&wd)).unwrap().count(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_writes_a_skill_the_loader_discovers() {
        let wd = scaffold("skills-create");
        let info = create(&wd, "my-skill", "My new skill", None, Some("# Guide\n\nDo it."), false).unwrap();
        assert_eq!(info.name, "my-skill");
        assert_eq!(info.kind, "instruction");
        assert!(!info.builtin && info.enabled);
        let text = fs::read_to_string(paths::skills_user_dir(&wd).join("my-skill/SKILL.md")).unwrap();
        assert!(text.contains("name: my-skill"));
        assert_eq!(load_full(&wd, "my-skill").unwrap(), "# Guide\n\nDo it.");
        assert!(short_list(&wd).contains(&("my-skill".to_string(), "My new skill".to_string())));
        assert!(create(&wd, "Bad Name", "x", None, None, false).is_err());
        assert!(create(&wd, "-lead", "x", None, None, false).is_err());
        let err = create(&wd, "my-skill", "dup", None, None, false).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
        // overwrite=true replaces the user copy
        create(&wd, "my-skill", "Replaced", None, Some("New body."), true).unwrap();
        assert_eq!(load_full(&wd, "my-skill").unwrap(), "New body.");
        // a builtin name is refused even with overwrite
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "builtin-one",
            "---\nname: builtin-one\ndescription: b\n---\nB.\n",
        );
        let err = create(&wd, "builtin-one", "x", None, None, true).unwrap_err().to_string();
        assert!(err.contains("conflicts with a builtin"), "{err}");
        assert!(create(&wd, "other", "d", Some("magic"), None, false).is_err());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn install_builtin_from_is_idempotent_and_never_touches_user_skills() {
        let wd = scaffold("skills-install-builtin");
        let src = wd.join("builtin-src");
        write_skill(&src, "ponytail", "---\nname: ponytail\ndescription: lazy mode\n---\nBe lazy.\n");
        write_skill(&src, "research", "---\nname: research\ndescription: research\n---\nSearch.\n");
        fs::create_dir_all(src.join("not-a-skill")).unwrap();
        fs::write(src.join("loose.txt"), "x").unwrap();
        write_skill(
            &paths::skills_user_dir(&wd),
            "ponytail",
            "---\nname: ponytail\ndescription: user copy\n---\nMine.\n",
        );

        let installed = install_builtin_from(&wd, &src).unwrap();
        assert_eq!(installed, ["ponytail", "research"]);
        let installed2 = install_builtin_from(&wd, &src).unwrap();
        assert_eq!(installed2, installed);
        assert_eq!(short_list(&wd).len(), 2); // user ponytail shadows the builtin copy
        assert_eq!(load_full(&wd, "ponytail").unwrap(), "Mine.");
        assert_eq!(load_full(&wd, "research").unwrap(), "Search.");

        // a refresh updates the builtin copy but never the user skill
        fs::write(src.join("ponytail/SKILL.md"), "---\nname: ponytail\ndescription: v2\n---\nNewer.\n").unwrap();
        install_builtin_from(&wd, &src).unwrap();
        assert_eq!(load_full(&wd, "ponytail").unwrap(), "Mine.");
        let builtin_copy =
            fs::read_to_string(paths::skills_builtin_dir(&wd).join("ponytail/SKILL.md")).unwrap();
        assert!(builtin_copy.contains("v2"), "{builtin_copy}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn builtin_home_source_prefers_kilo_dirs() {
        let home = temp_test_dir("skills-home");
        assert!(home_skill_source(Some(&home)).is_none());
        let kilocode = home.join(".kilocode/skills");
        write_skill(&kilocode, "demo", "---\nname: demo\ndescription: d\n---\nD.\n");
        assert_eq!(home_skill_source(Some(&home)), Some(kilocode));
        let config = home.join(".config/kilo/skills");
        fs::create_dir_all(&config).unwrap();
        assert_eq!(home_skill_source(Some(&home)), Some(config));
        fs::remove_dir_all(&home).unwrap();
    }

    fn bundled_repo_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("skills")
            .join(paths::SKILLS_BUILTIN)
    }

    #[test]
    fn bundled_repo_resources_are_non_empty_and_all_parse() {
        let src = bundled_repo_dir();
        assert!(src.is_dir(), "missing bundled resources at {}", src.display());
        // the resolver must pick the repo resources in dev/test builds
        assert_eq!(resolve_builtin_source().as_deref(), Some(src.as_path()));
        let dirs = find_skill_dirs(&src);
        assert!(!dirs.is_empty(), "bundled skill set is empty");
        for dir in &dirs {
            validate_skill_dir(dir)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        }
    }

    #[test]
    fn install_builtin_copies_bundled_set_idempotently_and_keeps_user_skills() {
        let wd = scaffold("skills-install-bundled");
        let installed = install_builtin(&wd).unwrap();
        assert!(!installed.is_empty());
        // every copied skill parses through the loader and lands in the lists
        let set = discover(&wd);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        assert_eq!(set.skills.len(), installed.len());
        assert_eq!(short_list(&wd).len(), installed.len());
        // idempotent: a second run installs the same set
        assert_eq!(install_builtin(&wd).unwrap(), installed);
        // a user skill with a bundled name is never overwritten by installs
        let name = installed[0].clone();
        write_skill(
            &paths::skills_user_dir(&wd),
            &name,
            &format!("---\nname: {name}\ndescription: mine\n---\nMine.\n"),
        );
        install_builtin(&wd).unwrap();
        assert_eq!(load_full(&wd, &name).unwrap(), "Mine.");
        // and the bundled copy stays loadable under its own name
        assert!(paths::skills_builtin_dir(&wd)
            .join(&name)
            .join(paths::SKILL_FILE)
            .is_file());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_name_normalization_maps_fuzzy_requests_to_canonical_names() {
        assert_eq!(normalize_skill_name("grill.me"), "grill-me");
        assert_eq!(normalize_skill_name("  Grill_Me  "), "grill-me");
        assert_eq!(normalize_skill_name("GRILL ME"), "grill-me");
        assert_eq!(normalize_skill_name("grill..--me"), "grill-me");
        assert_eq!(normalize_skill_name("-p-"), "p");
    }

    #[test]
    fn load_full_and_skill_tool_accept_normalized_names_and_suggest_close_misses() {
        let wd = temp_test_dir("skills-normalize");
        let user = paths::skills_user_dir(&wd);
        write_skill(
            &user,
            "grill-me",
            "---\nname: grill-me\ndescription: interview\n---\nGrill body.\n",
        );
        write_skill(
            &user,
            "grill-with-docs",
            "---\nname: grill-with-docs\ndescription: interview plus docs\n---\nDocs.\n",
        );
        write_skill(&user, "ponytail", "---\nname: ponytail\ndescription: lazy\n---\nLazy.\n");

        assert_eq!(load_full(&wd, "grill.me").unwrap(), "Grill body.");
        assert_eq!(load_full(&wd, "Grill_Me").unwrap(), "Grill body.");
        assert_eq!(load_full(&wd, "grill-me").unwrap(), "Grill body.");

        let err = load_full(&wd, "gril").unwrap_err().to_string();
        assert!(err.contains("skill not found: gril"), "{err}");
        assert!(err.contains("closest: grill-me, grill-with-docs"), "{err}");
        assert!(!err.contains("ponytail"), "{err}");

        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let r = SkillTool.execute(&json!({ "name": "grill.me" }), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["name"], "grill-me");
        let r = SkillTool.execute(&json!({ "name": "gril" }), &ctx).unwrap();
        let e = r.error.unwrap();
        assert_eq!(e.code, "not_found");
        assert!(e.message.contains("closest: grill-me, grill-with-docs"), "{}", e.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn sync_builtin_copies_missing_refreshes_changed_and_keeps_everything_else() {
        let wd = scaffold("skills-sync-builtin");
        let src = wd.join("builtin-src");
        write_skill(&src, "one", "---\nname: one\ndescription: bundled one\n---\nB1.\n");
        write_skill(&src, "two", "---\nname: two\ndescription: bundled two\n---\nB2.\n");
        // an unknown builtin dir (e.g. an older bundle's skill) must survive
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "scrapling",
            "---\nname: scrapling\ndescription: keep me\n---\nOld.\n",
        );
        // a user skill with a bundled name must never be touched
        write_skill(
            &paths::skills_user_dir(&wd),
            "one",
            "---\nname: one\ndescription: user one\n---\nMine.\n",
        );

        let synced = sync_builtin_from(&wd, &src).unwrap();
        assert_eq!(synced, ["one", "two"]);
        let copy =
            fs::read_to_string(paths::skills_builtin_dir(&wd).join("one/SKILL.md")).unwrap();
        assert!(copy.contains("bundled one"), "{copy}");
        assert_eq!(load_full(&wd, "one").unwrap(), "Mine."); // the user copy still wins
        assert!(paths::skills_builtin_dir(&wd).join("scrapling/SKILL.md").is_file());
        assert_eq!(
            fs::read_to_string(paths::skills_builtin_version_file(&wd))
                .unwrap()
                .trim(),
            env!("CARGO_PKG_VERSION")
        );

        // idempotent: a second run copies nothing
        assert!(sync_builtin_from(&wd, &src).unwrap().is_empty());
        assert!(paths::skills_builtin_dir(&wd).join("scrapling/SKILL.md").is_file());

        // a changed bundled skill refreshes only its builtin copy
        fs::write(src.join("two/SKILL.md"), "---\nname: two\ndescription: v2\n---\nB2 v2.\n").unwrap();
        assert_eq!(sync_builtin_from(&wd, &src).unwrap(), ["two"]);
        let copy =
            fs::read_to_string(paths::skills_builtin_dir(&wd).join("two/SKILL.md")).unwrap();
        assert!(copy.contains("v2"), "{copy}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn sync_builtin_uses_the_bundled_set_marks_the_version_and_is_idempotent() {
        let wd = scaffold("skills-sync-bundled");
        let synced = sync_builtin(&wd).unwrap();
        assert!(!synced.is_empty());
        for name in &synced {
            assert!(find(&wd, name).is_ok(), "{name} did not load back");
        }
        assert_eq!(
            fs::read_to_string(paths::skills_builtin_version_file(&wd))
                .unwrap()
                .trim(),
            env!("CARGO_PKG_VERSION")
        );
        assert!(sync_builtin(&wd).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn create_skill_tool_creates_skill_and_returns_error_envelopes() {
        let wd = scaffold("skills-create-tool");
        let ctx = ToolCtx {
            working_dir: Some(wd.clone()),
            ..Default::default()
        };
        let tool = CreateSkillTool;

        let r = tool
            .execute(
                &json!({ "name": "agent-guide", "description": "Guide", "body": "Do it." }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["skill"]["name"], "agent-guide");
        assert_eq!(load_full(&wd, "agent-guide").unwrap(), "Do it.");
        assert!(short_list(&wd).contains(&("agent-guide".to_string(), "Guide".to_string())));

        let r = tool
            .execute(&json!({ "name": "Bad Name", "description": "x" }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");

        let r = tool
            .execute(&json!({ "name": "agent-guide", "description": "dup" }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");

        // overwrite=true replaces the user copy
        let r = tool
            .execute(
                &json!({ "name": "agent-guide", "description": "Guide v2", "body": "New.", "overwrite": true }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(load_full(&wd, "agent-guide").unwrap(), "New.");

        // a builtin name is refused even with overwrite
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "builtin-one",
            "---\nname: builtin-one\ndescription: b\n---\nB.\n",
        );
        let r = tool
            .execute(
                &json!({ "name": "builtin-one", "description": "x", "overwrite": true }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "permission_denied");
        fs::remove_dir_all(&wd).unwrap();
    }
}
