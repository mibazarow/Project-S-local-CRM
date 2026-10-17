use std::path::{Path, PathBuf};

// Single source of truth for the on-disk layout of the working directory.

pub const PROJECTS_DIR: &str = "Projects";
pub const HARNESS_DIR: &str = "Harness";
pub const SETTINGS_DIR: &str = "Settings";

pub const SETTINGS_CONFIG: &str = "config";
pub const SETTINGS_SKILLS: &str = "skills";
pub const SETTINGS_MODELS: &str = "models";
pub const SETTINGS_DB: &str = ".db";
pub const SETTINGS_LOGS: &str = "logs";
pub const SETTINGS_BACKUPS: &str = "backups";

pub const HARNESS_TMP: &str = "tmp";
pub const HARNESS_INBOX: &str = "Входящие";

pub const PROJECT_TASKS: &str = "tasks";
pub const PROJECT_INBOX: &str = "Входящие";
pub const DATAS_MD: &str = "datas.md";

pub const DB_FILE: &str = "project-s.db";

pub const CONFIG_LLM: &str = "llm.json";
pub const CONFIG_APP: &str = "app.json";
pub const CONFIG_INDEXING: &str = "indexing.json";
pub const CONFIG_MCP: &str = "mcp.json";

pub const SKILLS_BUILTIN: &str = "builtin";
pub const SKILLS_USER: &str = "user";
pub const SKILLS_ENABLED: &str = "enabled.json";
pub const SKILL_FILE: &str = "SKILL.md";
// Written by skills::sync_builtin after each builtin-set sync.
pub const SKILLS_BUILTIN_VERSION: &str = ".builtin-version";

pub fn projects_dir(wd: &Path) -> PathBuf {
    wd.join(PROJECTS_DIR)
}

pub fn harness_dir(wd: &Path) -> PathBuf {
    wd.join(HARNESS_DIR)
}

pub fn harness_tmp_dir(wd: &Path) -> PathBuf {
    wd.join(HARNESS_DIR).join(HARNESS_TMP)
}

// Default landing folder for harness-session chat attachments (SPEC 6.1).
pub fn harness_inbox_dir(wd: &Path) -> PathBuf {
    wd.join(HARNESS_DIR).join(HARNESS_INBOX)
}

pub fn settings_dir(wd: &Path) -> PathBuf {
    wd.join(SETTINGS_DIR)
}

pub fn config_dir(wd: &Path) -> PathBuf {
    settings_dir(wd).join(SETTINGS_CONFIG)
}

pub fn skills_dir(wd: &Path) -> PathBuf {
    settings_dir(wd).join(SETTINGS_SKILLS)
}

pub fn skills_builtin_dir(wd: &Path) -> PathBuf {
    skills_dir(wd).join(SKILLS_BUILTIN)
}

pub fn skills_user_dir(wd: &Path) -> PathBuf {
    skills_dir(wd).join(SKILLS_USER)
}

// Single plaintext file listing disabled skill names; hand-editable and
// watcher-friendly (SPEC 6.2 "тумблеры вкл/выкл", "принцип открытости").
pub fn skills_enabled_file(wd: &Path) -> PathBuf {
    skills_dir(wd).join(SKILLS_ENABLED)
}

// Marker holding the app version that last synced the builtin skill set.
pub fn skills_builtin_version_file(wd: &Path) -> PathBuf {
    skills_dir(wd).join(SKILLS_BUILTIN_VERSION)
}

pub fn models_dir(wd: &Path) -> PathBuf {
    settings_dir(wd).join(SETTINGS_MODELS)
}

pub fn db_dir(wd: &Path) -> PathBuf {
    settings_dir(wd).join(SETTINGS_DB)
}

pub fn logs_dir(wd: &Path) -> PathBuf {
    settings_dir(wd).join(SETTINGS_LOGS)
}

pub fn backups_dir(wd: &Path) -> PathBuf {
    settings_dir(wd).join(SETTINGS_BACKUPS)
}

pub fn db_path(wd: &Path) -> PathBuf {
    db_dir(wd).join(DB_FILE)
}

// Directories the app creates when scaffolding a working dir.
pub fn scaffold_dirs(wd: &Path) -> Vec<PathBuf> {
    vec![
        projects_dir(wd),
        harness_dir(wd),
        harness_tmp_dir(wd),
        settings_dir(wd),
        config_dir(wd),
        skills_dir(wd),
        db_dir(wd),
        logs_dir(wd),
        backups_dir(wd),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_under_working_dir() {
        let wd = Path::new("/tmp/wd");
        assert_eq!(projects_dir(wd), PathBuf::from("/tmp/wd/Projects"));
        assert_eq!(harness_tmp_dir(wd), PathBuf::from("/tmp/wd/Harness/tmp"));
        assert_eq!(config_dir(wd), PathBuf::from("/tmp/wd/Settings/config"));
        assert_eq!(skills_dir(wd), PathBuf::from("/tmp/wd/Settings/skills"));
        assert_eq!(skills_builtin_dir(wd), PathBuf::from("/tmp/wd/Settings/skills/builtin"));
        assert_eq!(skills_user_dir(wd), PathBuf::from("/tmp/wd/Settings/skills/user"));
        assert_eq!(
            skills_enabled_file(wd),
            PathBuf::from("/tmp/wd/Settings/skills/enabled.json")
        );
        assert_eq!(logs_dir(wd), PathBuf::from("/tmp/wd/Settings/logs"));
        assert_eq!(backups_dir(wd), PathBuf::from("/tmp/wd/Settings/backups"));
        assert_eq!(db_path(wd), PathBuf::from("/tmp/wd/Settings/.db/project-s.db"));
    }

    #[test]
    fn scaffold_dirs_cover_full_layout() {
        let dirs = scaffold_dirs(Path::new("/tmp/wd"));
        assert_eq!(dirs.len(), 9);
        assert!(dirs.contains(&db_dir(Path::new("/tmp/wd"))));
        assert!(dirs.contains(&harness_tmp_dir(Path::new("/tmp/wd"))));
    }
}
