// Builtin skill set install and startup sync: resolve the bundled source
// (repo resources, packaged resources, or the kilo dev skill folders), copy
// the set into Settings/skills/builtin/, and refresh/remove stale copies on
// startup without ever touching Settings/skills/user/.

use crate::error::{AppError, Result};
use crate::paths;
use super::fs_util::copy_dir_all;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::fs_util::{find_skill_dirs, validate_skill_dir};
    use crate::onboarding::temp_test_dir;
    use crate::skills::manifest::discover;
    use crate::skills::testutil::{scaffold, write_skill};
    use crate::skills::{find, load_full, short_list};

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
}
