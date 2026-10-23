// SSH profile storage: Settings/config/ssh.json plus the current project's
// datas.md ("## SSH <name>" sections), merged so a datas entry replaces a
// config entry of the same name. Config types and the datas.md parser live
// here; the merged profile list feeds target resolution and ssh_list_profiles.
use super::command::{valid_host_or_user, DEFAULT_PORT};
use crate::agent::project_tools::{require_wd, with_conn};
use crate::agent::task_glue::read_datas;
use crate::agent::tool::ToolCtx;
use crate::config::{self, ConfigKind};
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;

// Settings/config/ssh.json shape: { "profiles": [ { name, host, user,
// port, keyPath, hostKeyPolicy, password } ] }. Unknown keys (top-level and
// per-profile) survive typed round-trips like the other configs (SPEC 3.4).
// `source` ("config" | "datas") is set by the loaders, never stored in
// ssh.json (skip_serializing keeps the file clean).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshConfig {
    pub profiles: Vec<SshProfile>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SshProfile {
    pub name: String,
    pub host: String,
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub key_path: Option<String>,
    // accept-new (default) | strict | off: mapped to ssh's
    // StrictHostKeyChecking option; anything else is a config error.
    #[serde(default)]
    pub host_key_policy: Option<String>,
    // Never returned by any tool, never placed in argv or errors: password
    // auth feeds it to a 0600 askpass file under Harness/tmp/ only.
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default, skip_serializing)]
    pub source: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

pub(super) fn load_config(wd: &Path) -> Result<SshConfig> {
    let value = config::read_config(wd, ConfigKind::Ssh)?;
    let mut cfg: SshConfig = serde_json::from_value(value)
        .map_err(|e| AppError::Invalid(format!("Settings/config/ssh.json is invalid: {e}")))?;
    for p in &mut cfg.profiles {
        p.source = "config".into();
    }
    Ok(cfg)
}

// datas.md SSH profile format (the user's chosen storage for access data):
//
//   ## SSH <name>
//   host: example.ru
//   user: deploy
//   port: 2222
//   password: s3cret
//   key: ~/.ssh/id_ed25519
//
// The heading is matched case-insensitively ("## ssh prod") and the section
// runs to the next "##" heading or EOF. Hand-written sections are accepted
// too: any other non-empty "## <title>" heading starts a candidate whose
// name is the title ("## Доступ к серверу"), field lines may carry bullet
// markers ("- host: ...") and Russian labels ("Хост:", "Пользователь:",
// "Порт:", "Пароль:", "Ключ:") map to the canonical keys, so a section the
// user wrote by hand is usable without reformatting. Field names are
// case-insensitive ("Host:", "PASSWORD:"); "key" and "keyPath" are both
// accepted. Any other text and all other sections are ignored. An entry is
// malformed (skipped whole) when its name, host or user is missing/invalid
// or its port line is unparsable; a missing port defaults to 22.
pub(crate) fn parse_datas_ssh(text: &str) -> Vec<SshProfile> {
    let mut profiles: Vec<SshProfile> = Vec::new();
    let mut current: Option<(String, Vec<(String, String)>)> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("##") {
            if let Some(p) = current.take().and_then(build_datas_profile) {
                profiles.push(p);
            }
            let rest = trimmed.trim_start_matches('#').trim();
            let mut words = rest.splitn(2, char::is_whitespace);
            let head = words.next().unwrap_or("");
            let tail = words.next().unwrap_or("").trim().to_string();
            if head.eq_ignore_ascii_case("ssh") {
                if !tail.is_empty() {
                    current = Some((tail, Vec::new()));
                }
            } else if !rest.is_empty() {
                current = Some((rest.to_string(), Vec::new()));
            }
            continue;
        }
        if let Some((_, fields)) = &mut current {
            let line = trimmed.trim_start_matches(['-', '*', '+']).trim();
            if let Some((key, value)) = line.split_once(':') {
                if let Some(key) = normalize_datas_key(&key.trim().to_lowercase()) {
                    fields.push((key.to_string(), value.trim().to_string()));
                }
            }
        }
    }
    if let Some(p) = current.and_then(build_datas_profile) {
        profiles.push(p);
    }
    profiles
}

// Canonical field key for a datas.md line label, English or Russian; None
// for anything that is not a connection field.
fn normalize_datas_key(key: &str) -> Option<&'static str> {
    Some(match key {
        "host" | "хост" => "host",
        "user" | "пользователь" => "user",
        "port" | "порт" => "port",
        "password" | "пароль" => "password",
        "key" | "keypath" | "ключ" => "key",
        _ => return None,
    })
}

// Turns one parsed section into a profile; None when the entry is malformed.
fn build_datas_profile((name, fields): (String, Vec<(String, String)>)) -> Option<SshProfile> {
    let get = |key: &str| {
        fields
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let host = get("host").filter(|h| valid_host_or_user(h))?;
    let user = get("user").filter(|u| valid_host_or_user(u))?;
    let port = match get("port") {
        None => DEFAULT_PORT,
        Some(raw) => match raw.parse::<u16>() {
            Ok(p) if (1..=65535).contains(&p) => p,
            _ => return None,
        },
    };
    Some(SshProfile {
        name,
        host,
        user,
        port,
        key_path: get("key").or_else(|| get("keypath")).filter(|k| !k.is_empty()),
        host_key_policy: None,
        password: get("password").filter(|p| !p.is_empty()),
        source: "datas".into(),
        extra: Map::new(),
    })
}

// The exact heading title under which a profile named `name` already lives
// in datas.md (case-insensitive match so a hand-written "## ssh prod" is
// updated in place instead of duplicated; a hand-written section named just
// after the profile, "## Доступ к серверу", is matched too); the canonical
// form otherwise.
pub(super) fn datas_section_title(md: &str, name: &str) -> String {
    let want = format!("ssh {name}");
    for line in md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("##") {
            let rest = trimmed.trim_start_matches('#').trim();
            if rest.eq_ignore_ascii_case(&want) || rest.eq_ignore_ascii_case(name) {
                return rest.to_string();
            }
        }
    }
    format!("SSH {name}")
}

// Profiles parsed from the current project's datas.md; any missing piece
// (no project, no state, unreadable file) simply yields no profiles.
pub(super) fn datas_profiles(ctx: &ToolCtx) -> Vec<SshProfile> {
    let Some(pid) = ctx.project_id else {
        return Vec::new();
    };
    let Ok(wd) = require_wd(ctx) else {
        return Vec::new();
    };
    let Ok(text) = with_conn(ctx, |conn| read_datas(conn, &wd, pid)) else {
        return Vec::new();
    };
    parse_datas_ssh(&text)
}

// ssh.json profiles merged with the current project's datas.md profiles;
// a datas entry replaces a config entry of the same name (datas.md is the
// user's chosen storage).
pub(super) fn all_profiles(ctx: &ToolCtx) -> Result<Vec<SshProfile>> {
    let wd = require_wd(ctx)?;
    let mut profiles = load_config(&wd)?.profiles;
    for p in datas_profiles(ctx) {
        match profiles.iter_mut().find(|existing| existing.name == p.name) {
            Some(existing) => *existing = p,
            None => profiles.push(p),
        }
    }
    Ok(profiles)
}

// Auth kind reported by ssh_list_profiles; a profile without an explicit
// key or password still authenticates through ssh-agent, i.e. by key.
pub(super) fn auth_kind(p: &SshProfile) -> &'static str {
    match (p.password.is_some(), p.key_path.is_some()) {
        (true, true) => "key+password",
        (true, false) => "password",
        (false, _) => "key",
    }
}
