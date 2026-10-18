// SKILL.md parsing and state (SPEC 6.2): the minimal YAML-subset frontmatter
// parser, discovery of the Settings/skills/{builtin,user} trees with conflict
// and error reporting, the enabled.json state, lenient name lookup, the
// full-body loader and the on-demand `skill` tool.

use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::paths;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

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
pub(crate) fn split_frontmatter(text: &str) -> (Option<Value>, &str) {
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

pub(crate) fn manifest_from_frontmatter(
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

// The entrypoint declared in the frontmatter: `entry:` or the first
// `scripts:` item (a plain string or a mapping with a name). None when the
// manifest declares none.
fn entrypoint_from_frontmatter(fm: &Value) -> Option<String> {
    let from_entry = fm.get("entry").and_then(Value::as_str).map(str::trim);
    let from_scripts = fm
        .get("scripts")
        .and_then(Value::as_array)
        .and_then(|scripts| scripts.first())
        .and_then(|s| s.as_str().or_else(|| s.get("name").and_then(Value::as_str)))
        .map(str::trim);
    from_entry
        .or(from_scripts)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// The entrypoint a skill declares, read from its SKILL.md frontmatter.
// skill_exec refuses skills without one.
pub fn entrypoint(skill: &SkillManifest) -> Option<String> {
    let text = fs::read_to_string(&skill.path).ok()?;
    entrypoint_from_frontmatter(split_frontmatter(&text).0.as_ref()?)
}

// Full SKILL.md body without the frontmatter. Script skills get a one-line
// entrypoint note pointing at skill_exec.
pub fn body(skill: &SkillManifest) -> Result<String> {
    let text = fs::read_to_string(&skill.path)?;
    let (fm, body) = split_frontmatter(&text);
    let body = body.trim().to_string();
    if let SkillKind::Script { timeout_sec } = skill.kind {
        let entry = fm
            .as_ref()
            .and_then(entrypoint_from_frontmatter)
            .unwrap_or_else(|| "scripts/".to_string());
        return Ok(format!(
            "> Skill script: entrypoint — {entry}, timeout {timeout_sec}s. Run it with skill_exec(name, args). Below is the full instruction text.\n\n{body}"
        ));
    }
    Ok(body)
}

pub fn load_full(wd: &Path, name: &str) -> Result<String> {
    body(&find(wd, name)?)
}

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

pub(crate) fn skill_info(manifest: &SkillManifest) -> SkillInfo {
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

// The skills-screen list: winners plus one error row per invalid skill and
// per builtin shadowed by a user skill.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use crate::skills::testutil::write_skill;

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
        assert!(text.contains("skill_exec"), "{text}");
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
}
