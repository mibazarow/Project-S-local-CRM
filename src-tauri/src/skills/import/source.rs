// The import entry point: resolve a folder/.zip/git source into candidate
// skill folders, install them into Settings/skills/user/ and clean up the
// staging folder.

use crate::error::{AppError, Result};
use crate::paths;
use super::archive::{extract_zip, run_git};
use super::fs_util::{find_skill_dirs, same_path, unique_suffix, validate_skill_dir, copy_dir_all};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub name: String,
    pub ok: bool,
    pub error: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::archive::run_git;
    use crate::skills::testutil::{scaffold, write_skill};
    use crate::skills::load_full;

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
}
