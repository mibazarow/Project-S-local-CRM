use crate::error::{AppError, Result};
use crate::paths;
use crate::state::AppState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::State;

const RELEASES_URL: &str = "https://gitverse.ru/mibazarow/Project-S-local-CRM/releases";
const CHECK_INTERVAL_SECS: i64 = 8 * 60 * 60;
const LOG_LIMIT: usize = 3;
const LOG_FILE: &str = "update-check.json";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current: String,
    pub latest: Option<String>,
    pub url: Option<String>,
    pub available: bool,
    pub checked_at: String,
    pub cached: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct UpdateLog {
    checks: Vec<UpdateCheck>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheck {
    checked_at: String,
    current: String,
    latest: Option<String>,
    url: Option<String>,
    available: bool,
    error: Option<String>,
}

fn log_path(wd: &Path) -> PathBuf {
    paths::settings_dir(wd).join(LOG_FILE)
}

fn read_log(wd: &Path) -> UpdateLog {
    std::fs::read_to_string(log_path(wd))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_log(wd: &Path, log: &UpdateLog) {
    let path = log_path(wd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(log) {
        let _ = std::fs::write(path, text);
    }
}

fn fresh(last: &UpdateCheck) -> bool {
    DateTime::parse_from_rfc3339(&last.checked_at)
        .map(|t| Utc::now().signed_duration_since(t).num_seconds() < CHECK_INTERVAL_SECS)
        .unwrap_or(false)
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let core = value.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn latest_from_html(html: &str) -> Option<String> {
    const MARKER: &str = "releases/tag/v";
    let mut best: Option<((u64, u64, u64), String)> = None;
    let mut rest = html;
    while let Some(at) = rest.find(MARKER) {
        let after = &rest[at + MARKER.len()..];
        let raw: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Some(version) = parse_version(&raw) {
            if best.as_ref().map_or(true, |(known, _)| version > *known) {
                best = Some((version, raw));
            }
        }
        rest = after;
    }
    best.map(|(_, raw)| raw)
}

fn fetch_latest() -> std::result::Result<(String, String), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(10))
        .build();
    let body = agent
        .get(RELEASES_URL)
        .set("User-Agent", "Project-S-update-check")
        .call()
        .map_err(|e| format!("update check request: {e}"))?
        .into_string()
        .map_err(|e| format!("update check body: {e}"))?;
    let latest = latest_from_html(&body).ok_or_else(|| "no release tag on the releases page".to_string())?;
    let url = format!("{RELEASES_URL}/tag/v{latest}");
    Ok((latest, url))
}

fn check_blocking(wd: &Path) -> Result<UpdateInfo> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let mut log = read_log(wd);

    if let Some(last) = log.checks.last() {
        if fresh(last) {
            return Ok(UpdateInfo {
                current: last.current.clone(),
                latest: last.latest.clone(),
                url: last.url.clone(),
                available: last.available,
                checked_at: last.checked_at.clone(),
                cached: true,
            });
        }
    }

    let checked_at = Utc::now().to_rfc3339();
    let (latest, url, available, error) = match fetch_latest() {
        Ok((latest, url)) => {
            let available = is_newer(&latest, &current);
            (Some(latest), Some(url), available, None)
        }
        Err(error) => (None, None, false, Some(error)),
    };

    log.checks.push(UpdateCheck {
        checked_at: checked_at.clone(),
        current: current.clone(),
        latest: latest.clone(),
        url: url.clone(),
        available,
        error,
    });
    if log.checks.len() > LOG_LIMIT {
        let excess = log.checks.len() - LOG_LIMIT;
        log.checks.drain(0..excess);
    }
    write_log(wd, &log);

    Ok(UpdateInfo {
        current,
        latest,
        url,
        available,
        checked_at,
        cached: false,
    })
}

#[tauri::command]
pub(crate) async fn check_update(state: State<'_, AppState>) -> Result<UpdateInfo> {
    let wd = state.working_dir()?;
    tauri::async_runtime::spawn_blocking(move || check_blocking(&wd))
        .await
        .map_err(|e| AppError::Io(format!("update check task: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_and_compares_them() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert!(is_newer("1.1.1", "1.1.0"));
        assert!(is_newer("1.2.0", "1.1.9"));
        assert!(!is_newer("1.1.0", "1.1.0"));
        assert!(!is_newer("1.0.9", "1.1.0"));
    }

    #[test]
    fn reads_the_highest_release_tag_from_the_page() {
        let html = r#"
            <a href="/mibazarow/Project-S-local-CRM/releases/tag/v1.1.0">1.1.0</a>
            <a href="/mibazarow/Project-S-local-CRM/releases/tag/v1.0.9">1.0.9</a>
            <a href="/mibazarow/Project-S-local-CRM/releases/tag/v1.2.0">1.2.0</a>
        "#;
        assert_eq!(latest_from_html(html).as_deref(), Some("1.2.0"));
        assert_eq!(latest_from_html("no releases"), None);
    }

    #[test]
    fn log_keeps_only_the_last_three_checks() {
        let mut log = UpdateLog::default();
        for i in 0..5 {
            log.checks.push(UpdateCheck {
                checked_at: Utc::now().to_rfc3339(),
                current: "1.1.0".into(),
                latest: Some(format!("1.1.{i}")),
                url: None,
                available: false,
                error: None,
            });
        }
        let excess = log.checks.len() - LOG_LIMIT;
        log.checks.drain(0..excess);
        assert_eq!(log.checks.len(), 3);
        assert_eq!(log.checks.last().unwrap().latest.as_deref(), Some("1.1.4"));
    }
}
