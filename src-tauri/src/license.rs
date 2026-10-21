use crate::error::Result;
use crate::onboarding;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const LICENSE_VERSION: u32 = 1;
const FILE_NAME: &str = "license-accepted.json";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseStatus {
    pub required: bool,
    pub accepted: bool,
    pub version: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LicenseState {
    accepted: bool,
    version: u32,
}

fn state_file(app_config_dir: &Path) -> PathBuf {
    app_config_dir.join(FILE_NAME)
}

fn read_state(app_config_dir: &Path) -> LicenseState {
    fs::read_to_string(state_file(app_config_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn is_accepted(state: &LicenseState) -> bool {
    state.accepted && state.version == LICENSE_VERSION
}

pub fn status(app_config_dir: &Path) -> LicenseStatus {
    let accepted = is_accepted(&read_state(app_config_dir));
    let configured = onboarding::status(app_config_dir).configured;
    LicenseStatus {
        required: !configured && !accepted,
        accepted,
        version: LICENSE_VERSION,
    }
}

pub fn accept(app_config_dir: &Path) -> Result<LicenseStatus> {
    fs::create_dir_all(app_config_dir)?;
    let state = LicenseState {
        accepted: true,
        version: LICENSE_VERSION,
    };
    fs::write(
        state_file(app_config_dir),
        serde_json::to_string_pretty(&state)? + "\n",
    )?;
    Ok(status(app_config_dir))
}

pub fn decline(exit: impl FnOnce()) {
    exit();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;

    #[test]
    fn new_install_requires_acceptance() {
        let root = temp_test_dir("license-new");
        let app_cfg = root.join("app-config");

        let st = status(&app_cfg);
        assert!(st.required);
        assert!(!st.accepted);
        assert_eq!(st.version, LICENSE_VERSION);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn configured_install_never_requires_acceptance() {
        let root = temp_test_dir("license-configured");
        let app_cfg = root.join("app-config");
        let wd = root.join("wd");
        fs::create_dir_all(&wd).unwrap();
        onboarding::init_working_dir(&app_cfg, &wd).unwrap();

        let st = status(&app_cfg);
        assert!(!st.required);
        assert!(!st.accepted);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn accept_persists_in_app_config_dir_and_drops_the_gate() {
        let root = temp_test_dir("license-accept");
        let app_cfg = root.join("app-config");
        let wd = root.join("wd");

        let st = accept(&app_cfg).unwrap();
        assert!(st.accepted);
        assert!(!st.required);
        assert!(state_file(&app_cfg).is_file());
        assert!(!wd.join(FILE_NAME).exists());

        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(state_file(&app_cfg)).unwrap()).unwrap();
        assert_eq!(raw["accepted"], true);
        assert_eq!(raw["version"], LICENSE_VERSION);

        // Acceptance survives a fresh status read (per app config, not per wd).
        assert!(status(&app_cfg).accepted);
        assert!(!status(&app_cfg).required);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn acceptance_of_an_older_version_does_not_count() {
        let root = temp_test_dir("license-version");
        let app_cfg = root.join("app-config");
        fs::create_dir_all(&app_cfg).unwrap();
        fs::write(
            state_file(&app_cfg),
            format!(r#"{{ "accepted": true, "version": {} }}"#, LICENSE_VERSION - 1),
        )
        .unwrap();

        let st = status(&app_cfg);
        assert!(!st.accepted);
        assert!(st.required);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn decline_takes_the_exit_path_without_accepting() {
        let root = temp_test_dir("license-decline");
        let app_cfg = root.join("app-config");

        let mut exited = false;
        decline(|| exited = true);
        assert!(exited);
        assert!(status(&app_cfg).required);
        assert!(!state_file(&app_cfg).exists());

        fs::remove_dir_all(&root).unwrap();
    }
}
