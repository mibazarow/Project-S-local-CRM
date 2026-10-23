// Skill management (SPEC 6.2): the backend behind the skills screen —
// import (folder/.zip/git), builtin set install and startup sync, user
// skill creation, and the `create_skill` agent tool. lib.rs wraps the
// plain functions as tauri commands.

use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::paths;
use super::manifest::{
    discover, manifest_from_frontmatter, skill_info, split_frontmatter, SkillInfo, SkillManifest,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
            // Windows/NSIS: bundled resources keep their "resources/skills"
            // prefix under the install dir (which is resource_dir()).
            let resource = exe_dir
                .join("resources")
                .join("skills")
                .join(paths::SKILLS_BUILTIN);
            if resource.is_dir() {
                return Some(resource);
            }
        }
    }
    // 3. Dev fallback: the standard kilo skill folders.
    let home = crate::platform::home_dir();
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

// Independent marker for the shipped builtin skill set: bump whenever the
// bundled set changes (skills added or removed) so existing installs can
// tell which builtin set they hold, without relying on the app version.
// Recorded in Settings/skills/.builtin-version by sync_builtin.
pub const BUILTIN_SET_VERSION: &str = "2";

// Startup sync of the bundled builtin set into an EXISTING working dir:
// copies only missing or changed builtin skill folders (SKILL.md content
// differs byte-for-byte) and removes builtin dirs the bundle no longer
// ships, so skills dropped from the delivery disappear on existing
// installs. Cleanup only touches app-managed copies (dirs containing a
// SKILL.md) under Settings/skills/builtin/; dotfiles, unrelated files and
// Settings/skills/user/ are never touched. Idempotent; the builtin set
// version is recorded in Settings/skills/.builtin-version.
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
    let mut shipped: HashSet<String> = HashSet::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !path.is_dir() || !path.join(paths::SKILL_FILE).is_file() {
            continue;
        }
        shipped.insert(name.clone());
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
    if let Ok(entries) = fs::read_dir(&dest_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !path.is_dir() || !path.join(paths::SKILL_FILE).is_file() {
                continue;
            }
            if !shipped.contains(&name) {
                fs::remove_dir_all(&path)?;
            }
        }
    }
    fs::write(
        paths::skills_builtin_version_file(wd),
        format!("{BUILTIN_SET_VERSION}\n"),
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

// The `create_skill` agent tool: creates a user skill through the same
// `create` logic the UI uses. Mutating with the Check policy (SPEC 6.2 Q20):
// every agent-initiated skill creation goes through the standard
// confirmation dialog — no silent skill creation, whether the user asked
// for a skill in chat or accepted a repeat-detector proposal. It only ever
// writes inside Settings/skills/user/. An existing user skill is replaced
// only with an explicit overwrite=true; builtin names are always refused.
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
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        let name = args["name"].as_str().unwrap_or("?");
        Some(format!(
            "Создание скилла «{name}» в Settings/skills/user/ — агент пишет новый SKILL.md"
        ))
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
    use crate::skills::testutil::{scaffold, write_skill};
    use crate::skills::{find, load_full, short_list};

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

    // Q20 end-to-end policy check: a run that calls create_skill must pass
    // the confirmation gate (Check policy). Allow → the skill is created and
    // listed; deny → nothing is written.
    struct ScriptedBackend {
        rounds: std::sync::Mutex<std::collections::VecDeque<crate::llm::ChatOutcome>>,
    }

    impl crate::agent::r#loop::ChatBackend for ScriptedBackend {
        fn chat_stream(
            &self,
            _messages: &[crate::llm::ChatMessage],
            _tools: Option<&[crate::llm::ToolSpec]>,
            _params: &crate::llm::ChatParams,
            _on_event: &mut dyn FnMut(crate::llm::ChatEvent),
        ) -> Result<crate::llm::ChatOutcome> {
            self.rounds
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| AppError::Invalid("scripted backend exhausted".into()))
        }
    }

    fn create_skill_run(wd: &Path, confirmer: &dyn crate::confirm::Confirmer) -> String {
        use crate::agent::catalog::Catalog;
        use crate::agent::finish::FinishTaskTool;
        use crate::agent::r#loop as agent_loop;
        use crate::llm::{ChatMessage, ChatOutcome, ChatParams, ToolCall};
        use std::sync::Arc;

        let backend = ScriptedBackend {
            rounds: std::sync::Mutex::new(
                vec![
                    ChatOutcome {
                        content: None,
                        reasoning: None,
                        tool_calls: vec![ToolCall::new(
                            "c1".into(),
                            "create_skill".into(),
                            json!({ "name": "repeat-task", "description": "Repeated workflow", "body": "Steps." })
                                .to_string(),
                        )],
                        finish_reason: Some("tool_calls".into()),
                        usage: None,
                    },
                    ChatOutcome {
                        content: None,
                        reasoning: None,
                        tool_calls: vec![ToolCall::new(
                            "c2".into(),
                            "finish_task".into(),
                            json!({ "action": "finish", "answer": "готово" }).to_string(),
                        )],
                        finish_reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ]
                .into(),
            ),
        };
        let mut catalog = Catalog::new();
        catalog.register(Arc::new(FinishTaskTool), true).unwrap();
        catalog.register(Arc::new(CreateSkillTool), true).unwrap();
        let mut state = agent_loop::AgentState {
            backend: &backend,
            catalog: &mut catalog,
        };
        let config = agent_loop::AgentRunConfig {
            messages: vec![ChatMessage::system("sys"), ChatMessage::user("да, создай скилл")],
            max_steps: 8,
            limits: agent_loop::RunLimits::default(),
            allow_all: false,
            project_id: None,
            session_id: None,
            chat_id: None,
            params: ChatParams {
                model: "test-model".into(),
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                requires_reasoning_passback: false,
            },
            cancel: None,
            state: None,
            working_dir: Some(wd.to_path_buf()),
            notes_root: None,
        };
        let result = agent_loop::run(&mut state, &config, confirmer, |_| {}).unwrap();
        result.answer
    }

    #[test]
    fn confirmed_create_skill_run_creates_and_lists_the_skill() {
        let wd = scaffold("skills-create-confirmed");
        let answer = create_skill_run(&wd, &crate::confirm::AutoConfirmer::allowing(true));
        assert_eq!(answer, "готово");
        assert!(paths::skills_user_dir(&wd).join("repeat-task/SKILL.md").is_file());
        assert!(short_list(&wd)
            .contains(&("repeat-task".to_string(), "Repeated workflow".to_string())));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn denied_create_skill_run_writes_nothing() {
        let wd = scaffold("skills-create-denied");
        let answer = create_skill_run(&wd, &crate::confirm::DenyAllConfirmer);
        assert_eq!(answer, "готово");
        assert!(!paths::skills_user_dir(&wd).join("repeat-task/SKILL.md").exists());
        assert!(short_list(&wd).is_empty());
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
    fn bundled_set_ships_the_universal_skills_and_not_the_removed_ones() {
        let names: HashSet<String> = find_skill_dirs(&bundled_repo_dir())
            .iter()
            .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for kept in [
            "doc-digest",
            "meeting-protocol",
            "spreadsheet-analysis",
            "email-drafts",
            "study-notes",
            "grilling",
        ] {
            assert!(names.contains(kept), "bundled set is missing {kept}");
        }
        for gone in [
            "agent-orchestration",
            "claude-handoff",
            "git-guardrails-claude-code",
            "setup-matt-pocock-skills",
            "ask-matt",
            "loop-me",
            "wayfinder",
            "triage",
            "migrate-to-shoehorn",
            "setup-ts-deep-modules",
            "setup-pre-commit",
            "grill-me",
            "grill-with-docs",
        ] {
            assert!(!names.contains(gone), "bundled set still ships {gone}");
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
    fn sync_builtin_copies_missing_refreshes_changed_and_keeps_everything_else() {
        let wd = scaffold("skills-sync-builtin");
        let src = wd.join("builtin-src");
        write_skill(&src, "one", "---\nname: one\ndescription: bundled one\n---\nB1.\n");
        write_skill(&src, "two", "---\nname: two\ndescription: bundled two\n---\nB2.\n");
        // a builtin dir the bundle no longer ships is removed on sync
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "scrapling",
            "---\nname: scrapling\ndescription: stale\n---\nOld.\n",
        );
        // non-skill entries under the builtin root are never touched
        fs::create_dir_all(paths::skills_builtin_dir(&wd).join("not-a-skill")).unwrap();
        fs::write(
            paths::skills_builtin_dir(&wd).join("loose.txt"),
            "keep",
        )
        .unwrap();
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
        assert!(!paths::skills_builtin_dir(&wd).join("scrapling").exists());
        assert!(paths::skills_builtin_dir(&wd).join("not-a-skill").is_dir());
        assert!(paths::skills_builtin_dir(&wd).join("loose.txt").is_file());
        assert_eq!(
            fs::read_to_string(paths::skills_builtin_version_file(&wd))
                .unwrap()
                .trim(),
            BUILTIN_SET_VERSION
        );

        // idempotent: a second run copies nothing and deletes nothing more
        assert!(sync_builtin_from(&wd, &src).unwrap().is_empty());
        assert!(paths::skills_builtin_dir(&wd).join("not-a-skill").is_dir());

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
        for universal in [
            "doc-digest",
            "meeting-protocol",
            "spreadsheet-analysis",
            "email-drafts",
            "study-notes",
        ] {
            assert!(find(&wd, universal).is_ok(), "{universal} did not load back");
        }
        assert_eq!(
            fs::read_to_string(paths::skills_builtin_version_file(&wd))
                .unwrap()
                .trim(),
            BUILTIN_SET_VERSION
        );
        assert!(sync_builtin(&wd).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn sync_builtin_removes_stale_dirs_but_keeps_user_skills_and_current_builtins() {
        let wd = scaffold("skills-sync-stale");
        let src = wd.join("builtin-src");
        write_skill(&src, "one", "---\nname: one\ndescription: bundled one\n---\nB1.\n");
        write_skill(&src, "two", "---\nname: two\ndescription: bundled two\n---\nB2.\n");
        sync_builtin_from(&wd, &src).unwrap();
        // an app-managed builtin copy the bundle no longer ships disappears
        write_skill(
            &paths::skills_builtin_dir(&wd),
            "grill-me",
            "---\nname: grill-me\ndescription: dropped\n---\nOld.\n",
        );
        write_skill(
            &paths::skills_user_dir(&wd),
            "grill-me",
            "---\nname: grill-me\ndescription: my own\n---\nMine.\n",
        );
        assert!(sync_builtin_from(&wd, &src).unwrap().is_empty());
        assert!(!paths::skills_builtin_dir(&wd).join("grill-me").exists());
        assert_eq!(load_full(&wd, "grill-me").unwrap(), "Mine.");
        assert_eq!(load_full(&wd, "one").unwrap(), "B1.");
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
