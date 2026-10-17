use crate::config;
use crate::db;
use crate::error::Result;
use crate::paths;
use rusqlite::Connection;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

// Info returned after the working dir is initialized.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingDirInfo {
    pub working_dir: String,
    pub config_dir: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingStatus {
    pub configured: bool,
    pub working_dir: Option<String>,
}

pub fn default_working_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join("Documents").join("PROJECT-S")
}

// The chosen working dir is persisted outside the working dir itself,
// in the OS app-data location (the working dir is unknown on first launch).
fn state_file(app_config_dir: &Path) -> PathBuf {
    app_config_dir.join("working_dir.txt")
}

pub fn load_working_dir(app_config_dir: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(state_file(app_config_dir)).ok()?;
    let path = PathBuf::from(text.trim());
    (!path.as_os_str().is_empty()).then_some(path).filter(|p| p.is_dir())
}

fn save_working_dir(app_config_dir: &Path, wd: &Path) -> Result<()> {
    fs::create_dir_all(app_config_dir)?;
    fs::write(state_file(app_config_dir), wd.display().to_string())?;
    Ok(())
}

pub fn status(app_config_dir: &Path) -> OnboardingStatus {
    match load_working_dir(app_config_dir) {
        Some(wd) => OnboardingStatus {
            configured: true,
            working_dir: Some(wd.display().to_string()),
        },
        None => OnboardingStatus {
            configured: false,
            working_dir: None,
        },
    }
}

// Creates the full folder scaffold, default configs and the SQLite DB.
// Safe to run repeatedly.
pub fn init_working_dir(app_config_dir: &Path, wd: &Path) -> Result<(WorkingDirInfo, Connection)> {
    fs::create_dir_all(wd)?;
    // Notes vault first: ensure() detects a freshly created folder to seed
    // the welcome note, so it must run before the scaffold loop re-creates
    // Notes/ (idempotent there).
    crate::notes::ensure(wd)?;
    for dir in paths::scaffold_dirs(wd) {
        fs::create_dir_all(dir)?;
    }
    config::ensure_defaults(wd)?;
    // SPEC 6.2: the builtin coding-agent skill set ships with the app, so
    // Settings/skills/builtin/ is populated out of the box. Idempotent and
    // best-effort: a missing source never blocks onboarding, and user skills
    // are never touched.
    let _ = crate::skills::install_builtin(wd);
    save_working_dir(app_config_dir, wd)?;
    let conn = db::open(&paths::db_path(wd))?;
    let info = WorkingDirInfo {
        working_dir: wd.display().to_string(),
        config_dir: paths::config_dir(wd).display().to_string(),
    };
    Ok((info, conn))
}

#[cfg(test)]
pub(crate) fn temp_test_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("project-s-test-{tag}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_working_dir_is_documents_project_s() {
        let dir = default_working_dir();
        assert!(dir.ends_with("Documents/PROJECT-S"));
    }

    #[test]
    fn init_creates_full_layout_and_is_idempotent() {
        let root = temp_test_dir("scaffold");
        let app_cfg = root.join("app-config");
        let wd = root.join("wd");

        let (info, conn) = init_working_dir(&app_cfg, &wd).unwrap();
        let (info2, conn2) = init_working_dir(&app_cfg, &wd).unwrap();
        drop(conn);
        drop(conn2);

        assert_eq!(info.working_dir, info2.working_dir);
        for dir in [
            "Projects",
            "Harness",
            "Harness/tmp",
            "Settings",
            "Settings/config",
            "Settings/skills",
            "Settings/skills/builtin",
            "Settings/.db",
            "Settings/logs",
            "Settings/backups",
            "Notes",
        ] {
            assert!(wd.join(dir).is_dir(), "missing dir: {dir}");
        }
        // The fresh vault gets exactly the seeded welcome note, and a second
        // init must not duplicate or re-seed it.
        assert!(paths::notes_dir(&wd)
            .join(crate::notes::WELCOME_NOTE)
            .is_file());
        assert_eq!(fs::read_dir(paths::notes_dir(&wd)).unwrap().count(), 1);
        for file in ["llm.json", "app.json", "indexing.json", "mcp.json"] {
            assert!(paths::config_dir(&wd).join(file).is_file(), "missing file: {file}");
        }
        assert!(paths::db_path(&wd).is_file());
        assert!(paths::db_path(&wd).starts_with(&wd));

        assert_eq!(load_working_dir(&app_cfg), Some(wd.clone()));
        let st = status(&app_cfg);
        assert!(st.configured);
        assert_eq!(st.working_dir, Some(wd.display().to_string()));

        // Nothing is created in the working-dir root besides the four top folders.
        let mut root_entries: Vec<String> = fs::read_dir(&wd)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        root_entries.sort();
        assert_eq!(root_entries, ["Harness", "Notes", "Projects", "Settings"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn status_reports_missing_working_dir_as_unconfigured() {
        let root = temp_test_dir("onboarding-missing");
        let app_cfg = root.join("app-config");
        assert!(!status(&app_cfg).configured);

        let gone = root.join("gone");
        fs::create_dir_all(&app_cfg).unwrap();
        fs::write(state_file(&app_cfg), gone.display().to_string()).unwrap();
        assert!(!status(&app_cfg).configured);

        fs::remove_dir_all(&root).unwrap();
    }
}
