// Agent SSH tools via the system ssh/scp/ssh-keygen CLI (no new crates).
// Profiles live in Settings/config/ssh.json AND in the current project's
// datas.md ("## SSH <name>" sections, the user's chosen storage); both
// sources authenticate with keys/ssh-agent and passwords. Password auth
// goes through SSH_ASKPASS: the password lives only in a 0600 file under
// Harness/tmp/ next to a 0700 askpass script, both deleted when the call
// ends — it never enters argv, tool results or errors. ssh_exec is Mutating
// and CheckConditional: a clearly read-only command (safe first token, no
// shell metacharacters) skips confirmation, everything else confirms with
// the destructive-command analysis enriching the confirmation text.
// ssh_upload/ssh_download/ssh_generate_key/ssh_install_key stay always
// confirmed. Local paths stay inside
// the project root (project mode) or resolve like the harness fs tools
// (harness mode); remote paths pass through with newline/NUL rejection.
// Child processes see TMPDIR/TEMP/TMP pointed at Harness/tmp/ (SPEC 5.2).
// Output is secret-masked and capped; home paths are shortened to ~ in
// errors.
use crate::path_policy::join_rel;
use super::harness_tools::{canonicalish, resolve_path};
use super::project_tools::{envelope, require_wd, with_conn};
use super::task_glue::{read_datas, upsert_section, write_datas};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::config::{self, ConfigKind};
use crate::error::{AppError, Result};
use crate::journal::mask_secrets;
use crate::{paths, projects};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

const SSH_TIMEOUT: Duration = Duration::from_secs(120);
// ssh_exec timeoutSec bounds: long installs need more than the default 120 s,
// but the call is still capped so a hung session cannot wedge the agent loop.
const SSH_TIMEOUT_MIN_SECS: u64 = 10;
const SSH_TIMEOUT_MAX_SECS: u64 = 1800;
const SSH_OUTPUT_CAP: usize = 20_000;
// Envelope cap leaves room for the JSON wrapper around a full 20k output.
const SSH_RESULT_CAP: usize = 21_000;
const CONNECT_TIMEOUT_SECS: u32 = 10;
const DEFAULT_PORT: u16 = 22;

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

fn load_config(wd: &Path) -> Result<SshConfig> {
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
fn datas_section_title(md: &str, name: &str) -> String {
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
fn datas_profiles(ctx: &ToolCtx) -> Vec<SshProfile> {
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
fn all_profiles(ctx: &ToolCtx) -> Result<Vec<SshProfile>> {
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
fn auth_kind(p: &SshProfile) -> &'static str {
    match (p.password.is_some(), p.key_path.is_some()) {
        (true, true) => "key+password",
        (true, false) => "password",
        (false, _) => "key",
    }
}

// The resolved endpoint of one tool call. The password (when any) is only
// ever written to the askpass file: it never enters argv or results.
#[derive(Debug, Clone)]
struct Target {
    host: String,
    user: String,
    port: u16,
    key_path: Option<PathBuf>,
    password: Option<String>,
    strict_host_key: &'static str,
}

impl Target {
    fn label(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}

// Hosts and users land in one ssh argv element (user@host): whitespace,
// control characters, '@' and a leading '-' (option injection) are refused.
fn valid_host_or_user(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains('@')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(raw)
}

// Key paths are not secrets, but the user's home is theirs: show ~ instead.
fn shorten(path: &Path) -> String {
    let home = home_dir();
    if home != Path::new("/") && path.starts_with(&home) {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~{}{}", std::path::MAIN_SEPARATOR_STR, rest.display());
        }
    }
    path.display().to_string()
}

fn build_target(
    host: &str,
    user: &str,
    port: i64,
    key_path: Option<&str>,
    host_key_policy: Option<&str>,
    password: Option<String>,
) -> Result<Target> {
    if !valid_host_or_user(host) {
        return Err(AppError::Invalid(format!("invalid ssh host {host:?}")));
    }
    if !valid_host_or_user(user) {
        return Err(AppError::Invalid(format!("invalid ssh user {user:?}")));
    }
    if !(1..=65535).contains(&port) {
        return Err(AppError::Invalid(format!(
            "ssh port {port} is out of range 1-65535"
        )));
    }
    let strict_host_key = match host_key_policy {
        None | Some("accept-new") => "accept-new",
        Some("strict") => "yes",
        Some("off") => "no",
        Some(other) => {
            return Err(AppError::Invalid(format!(
                "invalid hostKeyPolicy {other:?} (expected accept-new, strict or off)"
            )))
        }
    };
    let key = key_path.map(expand_tilde);
    if let Some(k) = &key {
        if !k.is_file() {
            return Err(AppError::Invalid(format!(
                "ssh key file not found: {}",
                shorten(k)
            )));
        }
    }
    Ok(Target {
        host: host.to_string(),
        user: user.to_string(),
        port: port as u16,
        key_path: key,
        password: password.filter(|p| !p.is_empty()),
        strict_host_key,
    })
}

// Either a named profile from ssh.json or the current project's datas.md
// (the profile wins over explicit fields, and it carries its own
// password/key so the model never needs to see the secret) or explicit
// host/user/port/keyPath/password for ad-hoc use. host+user are required.
fn resolve_target(args: &Value, ctx: &ToolCtx) -> Result<Target> {
    let profile = args.get("profile").and_then(Value::as_str).unwrap_or("").trim();
    if !profile.is_empty() {
        let profiles = all_profiles(ctx)?;
        let p = profiles.iter().find(|p| p.name == profile).ok_or_else(|| {
            let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        let known = if names.is_empty() {
            "none (add a \"## SSH <name>\" section with host:/user: lines to the project's datas.md or a profile to Settings/config/ssh.json)".to_string()
        } else {
                names.join(", ")
            };
            AppError::Invalid(format!(
                "ssh profile \"{profile}\" not found; known profiles: {known}"
            ))
        })?;
        return build_target(
            &p.host,
            &p.user,
            i64::from(p.port),
            p.key_path.as_deref(),
            p.host_key_policy.as_deref(),
            p.password.clone(),
        );
    }
    let host = args.get("host").and_then(Value::as_str).unwrap_or("").trim();
    let user = args.get("user").and_then(Value::as_str).unwrap_or("").trim();
    if host.is_empty() || user.is_empty() {
        return Err(AppError::Invalid(
            "ssh target requires host and user (or a known profile)".into(),
        ));
    }
    let port = args
        .get("port")
        .and_then(Value::as_i64)
        .unwrap_or(i64::from(DEFAULT_PORT));
    let key_path = args.get("keyPath").and_then(Value::as_str);
    let password = args
        .get("password")
        .and_then(Value::as_str)
        .map(str::to_string);
    build_target(host, user, port, key_path, None, password)
}

// Missing profiles/keys are not_found, everything else about the target is
// a bad call from the model.
fn target_error(e: AppError) -> ToolResult {
    let msg = e.to_string();
    if msg.contains("not found") {
        ToolResult::err(ToolErrorCode::NotFound, msg)
    } else {
        ToolResult::err(ToolErrorCode::InvalidArgs, msg)
    }
}

fn target_label(args: &Value, ctx: &ToolCtx) -> String {
    resolve_target(args, ctx)
        .map(|t| t.label())
        .unwrap_or_else(|_| "цель не определена".to_string())
}

// Key-only auth: BatchMode=yes so ssh/scp can never fall back to a prompt.
// With a password: BatchMode=no plus exactly one prompt, answered through
// SSH_ASKPASS (see Askpass); when a key is present too, publickey is tried
// first and the password only as the fallback.
fn common_opts(t: &Target) -> Vec<String> {
    let batch = if t.password.is_some() { "no" } else { "yes" };
    let mut opts = vec![
        "-o".to_string(),
        format!("BatchMode={batch}"),
        "-o".to_string(),
        format!("StrictHostKeyChecking={}", t.strict_host_key),
        "-o".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
    ];
    if t.password.is_some() {
        opts.push("-o".to_string());
        opts.push("NumberOfPasswordPrompts=1".to_string());
        if t.key_path.is_some() {
            opts.push("-o".to_string());
            opts.push("PreferredAuthentications=publickey,password,keyboard-interactive".to_string());
        }
    }
    opts
}

// Password auth without ever putting the secret in argv: a private dir
// under Harness/tmp/ holds a 0600 password file and a 0700 askpass script
// that cats it; ssh reads the password from the script (SSH_ASKPASS with
// SSH_ASKPASS_REQUIRE=force, DISPLAY set so non-tty ssh uses askpass).
// The whole dir is removed when the guard drops — on success, failure or
// error alike.
const ASKPASS_DIR_PREFIX: &str = "ssh-askpass-";

struct Askpass {
    dir: PathBuf,
    script: PathBuf,
}

impl Askpass {
    fn create(wd: &Path, password: &str) -> Result<Askpass> {
        let tmp = paths::harness_tmp_dir(wd);
        fs::create_dir_all(&tmp)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| AppError::Io(format!("clock unavailable: {e}")))?
            .as_nanos();
        let dir = tmp.join(format!("{ASKPASS_DIR_PREFIX}{}-{nonce}", std::process::id()));
        fs::create_dir(&dir)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let pass = dir.join("pass");
        fs::write(&pass, password)?;
        fs::set_permissions(&pass, fs::Permissions::from_mode(0o600))?;
        let script = dir.join("askpass.sh");
        fs::write(
            &script,
            format!("#!/bin/sh\ncat {}\n", shell_quote(&pass.display().to_string())),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        Ok(Askpass { dir, script })
    }

    fn envs(&self) -> Vec<(String, String)> {
        vec![
            ("SSH_ASKPASS".to_string(), self.script.display().to_string()),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            ("DISPLAY".to_string(), ":0".to_string()),
        ]
    }
}

impl Drop for Askpass {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// Password auth for one tool call: Some(guard) while the process runs.
fn askpass_for(target: &Target, wd: &Path) -> Result<Option<Askpass>> {
    match &target.password {
        Some(pw) => Askpass::create(wd, pw).map(Some),
        None => Ok(None),
    }
}

// Startup hygiene (refactor S1.7): a crash between Askpass::create and the
// guard's Drop leaves the private dir behind under Harness/tmp/. On startup
// remove leftovers that cannot belong to a live run: entries directly under
// the tmp root named exactly ssh-askpass-<pid>-<nonce> whose pid is dead
// (kill -0, the same probe processes::reconcile uses) AND whose mtime is at
// least ASKPASS_STALE_AGE old. The age guard keeps a concurrent run of
// another instance safe even under pid reuse: ssh calls are capped at
// 30 minutes, well under an hour. Symlink entries are unlinked, never
// followed, and remove_dir_all unlinks links inside the tree without
// following them, so nothing outside Harness/tmp is ever entered or
// removed. Returns the number of removed entries (best-effort: failures
// just leave the entry for the next startup).
const ASKPASS_STALE_AGE: Duration = Duration::from_secs(3600);

pub(crate) fn purge_stale_askpass_dirs(wd: &Path) -> usize {
    let tmp = paths::harness_tmp_dir(wd);
    let Ok(entries) = fs::read_dir(&tmp) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(rest) = name_str.strip_prefix(ASKPASS_DIR_PREFIX) else {
            continue;
        };
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            // Askpass::create only ever makes real dirs, so a matching
            // symlink is never an active askpass dir: unlink the link.
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        // ssh-askpass-<pid>-<nonce>: anything else is not ours to touch.
        let Some((pid, nonce)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if nonce.is_empty() || pid == 0 || crate::processes::kill_alive(&pid.to_string()) {
            continue;
        }
        let stale = meta
            .modified()
            .map(|mtime| {
                SystemTime::now()
                    .duration_since(mtime)
                    .unwrap_or(Duration::ZERO)
                    >= ASKPASS_STALE_AGE
            })
            .unwrap_or(false);
        if stale && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn ssh_argv(t: &Target) -> Vec<String> {
    let mut argv = common_opts(t);
    argv.push("-p".to_string());
    argv.push(t.port.to_string());
    if let Some(key) = &t.key_path {
        argv.push("-i".to_string());
        argv.push(key.display().to_string());
    }
    argv
}

// POSIX single-quote wrapping: the only character that needs escaping inside
// single quotes is the quote itself, closed and reopened as '\''.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// Per-call ssh_exec options (defaults resolved in execute()).
struct ExecOpts {
    login_shell: bool,
    cwd: Option<String>,
    pty: bool,
    timeout: Duration,
}

// timeoutSec default 120 s, clamped to 10..1800 so a hung session cannot
// wedge the agent loop but long installs still get room.
fn clamp_timeout(value: Option<i64>) -> Duration {
    let secs = value
        .unwrap_or(SSH_TIMEOUT.as_secs() as i64)
        .clamp(SSH_TIMEOUT_MIN_SECS as i64, SSH_TIMEOUT_MAX_SECS as i64);
    Duration::from_secs(secs as u64)
}

// The single remote command string ssh hands to the remote shell. loginShell
// wraps the command in `bash -lc '<command>'` so PATH/profile apply; otherwise
// the command is passed through. A bare command that starts with '-' is wrapped
// in `sh -c` so the remote shell does not read it as its own option. An
// optional cwd is prepended as `cd '<cwd>' && ` (the path is shell-quoted).
fn remote_command(command: &str, login_shell: bool, cwd: Option<&str>) -> String {
    let body = if login_shell {
        format!("bash -lc {}", shell_quote(command))
    } else if command.starts_with('-') {
        format!("sh -c {}", shell_quote(command))
    } else {
        command.to_string()
    };
    match cwd {
        Some(dir) if !dir.is_empty() => format!("cd {} && {}", shell_quote(dir), body),
        _ => body,
    }
}

// OpenSSH stops option parsing at the destination, so everything after
// user@host is the remote command joined with spaces and run by the remote
// shell. There is no "--" here: it would reach the remote shell literally and
// break the command. The command stays a single trailing argv element.
fn ssh_exec_argv(t: &Target, command: &str, opts: &ExecOpts) -> Vec<String> {
    let mut argv = ssh_argv(t);
    if opts.pty {
        // -tt forces a pseudo-terminal even without one locally; interactive
        // programs may hang, but the call is still capped by opts.timeout.
        argv.push("-tt".to_string());
    }
    argv.push(format!("{}@{}", t.user, t.host));
    argv.push(remote_command(command, opts.login_shell, opts.cwd.as_deref()));
    argv
}

fn scp_argv(t: &Target, src: &str, dst: &str, recursive: bool) -> Vec<String> {
    let mut argv = common_opts(t);
    argv.push("-P".to_string());
    argv.push(t.port.to_string());
    if let Some(key) = &t.key_path {
        argv.push("-i".to_string());
        argv.push(key.display().to_string());
    }
    if recursive {
        argv.push("-r".to_string());
    }
    argv.push(src.to_string());
    argv.push(dst.to_string());
    argv
}

struct SshOutput {
    code: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
}

// Reads a pipe up to the cap plus one byte so truncation stays detectable
// while the process keeps producing.
fn read_capped(mut reader: impl Read) -> (String, bool) {
    let mut buf = Vec::new();
    let _ = (&mut reader)
        .take((SSH_OUTPUT_CAP as u64) + 1)
        .read_to_end(&mut buf);
    let truncated = buf.len() > SSH_OUTPUT_CAP;
    buf.truncate(SSH_OUTPUT_CAP);
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

// `ssh`/`scp`/`ssh-keygen` with piped output, TMPDIR/TEMP/TMP pointed at
// Harness/tmp/, a hard timeout (kill) and masked output. `extra_env` carries
// the askpass variables for password auth (never any secret in argv).
fn run_process(
    program: &str,
    argv: &[String],
    wd: &Path,
    timeout: Duration,
    extra_env: &[(String, String)],
) -> Result<SshOutput> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let mut command = Command::new(program);
    command
        .args(argv)
        .stdin(Stdio::null())
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = match command.spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(AppError::Io(format!("cannot run {program}: {e}"))),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || stdout.map(read_capped));
    let err_thread = std::thread::spawn(move || stderr.map(read_capped));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Io(format!(
                "{program} timed out after {} s and was killed",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
    let (stderr, err_cut) = err_thread.join().unwrap_or_default().unwrap_or_default();
    Ok(SshOutput {
        code: status.code().unwrap_or(-1),
        stdout: mask_secrets(&stdout),
        stderr: mask_secrets(&stderr),
        truncated: out_cut || err_cut,
    })
}

// Local paths: project mode keeps every path inside the project root
// (upload targets must exist, download destinations may be new); harness
// mode resolves absolute/~/wd-relative paths like the harness fs tools.
fn resolve_local_path(raw: &str, ctx: &ToolCtx, wd: &Path, must_exist: bool) -> Result<PathBuf> {
    if raw.is_empty() {
        return Err(AppError::Invalid("localPath is empty".into()));
    }
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            let joined = join_rel(&root, raw)?;
            let check = if must_exist {
                joined
                    .canonicalize()
                    .map_err(|_| AppError::Invalid(format!("\"{raw}\" not found")))?
            } else {
                canonicalish(&joined)
            };
            if !check.starts_with(&root) {
                return Err(AppError::Invalid(format!(
                    "\"{raw}\" escapes the project folder"
                )));
            }
            Ok(check)
        }
        None => {
            let path = resolve_path(raw, wd);
            if must_exist {
                path.canonicalize()
                    .map_err(|_| AppError::Invalid(format!("\"{raw}\" not found")))
            } else {
                Ok(path)
            }
        }
    }
}

// Remote paths pass through to scp unchanged, but never carry separators
// that could smuggle extra arguments into the remote shell.
fn check_remote_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(AppError::Invalid("remotePath is empty".into()));
    }
    if path.contains(['\n', '\r', '\0']) {
        return Err(AppError::Invalid(
            "remotePath must not contain newlines or NUL".into(),
        ));
    }
    Ok(())
}

// ---- read-only command allowlist (pure, decides the ssh_exec policy) ----

// Commands that only read remote state. Multi-word entries need both the
// program and its read-only subcommand/arguments matched exactly.
const READ_ONLY_COMMANDS: [&str; 23] = [
    "ls", "cat", "head", "tail", "grep", "find", "ps", "top", "df", "du", "free", "uptime",
    "whoami", "id", "pwd", "uname", "hostname", "date", "ss", "netstat", "wc", "stat", "file",
];
const READ_ONLY_COMMAND_PAIRS: [&str; 6] = [
    "git status",
    "git log",
    "git diff",
    "docker ps",
    "systemctl status",
    "ip addr",
];

// A command is clearly read-only only when its first token (or the first two
// for the multi-word entries) is on the allowlist AND it carries no shell
// metacharacters, redirections, substitutions or separators that could hide
// a second command: ; | > < & (covering && and ||), backticks, "$(", and
// newlines. Everything else stays on the confirmation side.
fn is_read_only_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || command.contains([';', '|', '>', '<', '&', '`', '\n', '\r']) {
        return false;
    }
    if command.contains("$(") {
        return false;
    }
    let mut words = command.split_whitespace();
    let first = words.next().unwrap_or("");
    if READ_ONLY_COMMANDS.contains(&first) {
        return true;
    }
    let second = words.next().unwrap_or("");
    READ_ONLY_COMMAND_PAIRS.contains(&format!("{first} {second}").as_str())
}

// ---- destructive-command labeling (pure, never blocks) ----

// The confirmation is the guard; this only explains WHY a command looks
// dangerous in the confirmation queue. It errs on the side of flagging: a
// false positive costs nothing extra (non-read-only commands confirm
// anyway).
const WRAPPERS: [&str; 5] = ["sudo", "env", "xargs", "timeout", "nohup"];

fn basename_word(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn is_duration(token: &str) -> bool {
    let digits = token.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn has_short_flag(args: &[String], letter: char) -> bool {
    args.iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(letter))
}

// Quotes and backslashes are blanked so `sh -c 'rm -rf /'` tokenises like
// the inner command; segments split on ; && || | and newlines; leading
// wrapper words (and their options, durations and assignments) are skipped
// to find the effective command word of each segment.
fn effective_commands(command: &str) -> Vec<(String, Vec<String>)> {
    let cleaned: String = command
        .chars()
        .map(|c| if matches!(c, '\'' | '"' | '\\') { ' ' } else { c })
        .collect();
    let mut out = Vec::new();
    for segment in cleaned.split([';', '|', '&', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let token = tokens[i];
            if is_assignment(token) {
                i += 1;
                continue;
            }
            let word = basename_word(token);
            if WRAPPERS.contains(&word) {
                i += 1;
                while i < tokens.len() {
                    let next = tokens[i];
                    if next.starts_with('-') || is_duration(next) || is_assignment(next) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            out.push((
                word.to_string(),
                tokens[i + 1..].iter().map(|t| t.to_string()).collect(),
            ));
            break;
        }
    }
    out
}

// `:(){` with optional whitespace between the parts (the fork bomb head).
fn contains_fork_bomb(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    'outer: for i in 0..chars.len() {
        if chars[i] != ':' {
            continue;
        }
        let mut j = i + 1;
        for want in ['(', ')', '{'] {
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j >= chars.len() || chars[j] != want {
                continue 'outer;
            }
            j += 1;
        }
        return true;
    }
    false
}

// Any redirect into /dev (device writes; also flags harmless 2>/dev/null —
// this is labeling for the confirmation text, not a block).
fn writes_to_dev(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'>' {
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }
        if bytes[j..].starts_with(b"/dev/") {
            return true;
        }
    }
    false
}

// Human-readable Russian reasons for every matched destructive pattern;
// empty when the command looks safe. Never blocks execution by itself.
pub(crate) fn destructive_reasons(command: &str) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    let mut add = |reason: &str| {
        if !reasons.iter().any(|r| r == reason) {
            reasons.push(reason.to_string());
        }
    };
    for (word, args) in effective_commands(command) {
        let word = word.as_str();
        if word == "rm"
            && (has_short_flag(&args, 'r')
                || has_short_flag(&args, 'R')
                || args.iter().any(|a| a == "--recursive"))
        {
            add("рекурсивное удаление (rm -r)");
        }
        if word == "dd" {
            add("низкоуровневая запись на диск (dd)");
        }
        if word.starts_with("mkfs") {
            add("форматирование файловой системы (mkfs)");
        }
        if matches!(word, "shutdown" | "reboot" | "poweroff" | "halt") {
            add("выключение или перезагрузка машины");
        }
        if word == "git"
            && args.iter().any(|a| a == "reset")
            && args.iter().any(|a| a == "--hard")
        {
            add("жёсткий сброс рабочего дерева git (git reset --hard)");
        }
        if word == "chmod"
            && (has_short_flag(&args, 'R') || args.iter().any(|a| a == "--recursive"))
        {
            add("рекурсивная смена прав (chmod -R)");
        }
        if matches!(word, "killall" | "pkill") {
            add("массовое завершение процессов (killall/pkill)");
        }
    }
    if contains_fork_bomb(command) {
        add("fork-бомба");
    }
    if writes_to_dev(command) {
        add("запись напрямую в /dev");
    }
    reasons
}

// ---- tools ----

pub struct SshListProfilesTool;

impl Tool for SshListProfilesTool {
    fn name(&self) -> &str {
        "ssh_list_profiles"
    }
    fn description(&self) -> &str {
        "List SSH profiles merged from Settings/config/ssh.json and the current project's datas.md (\"## SSH <name>\" sections; hand-written sections with host/user fields under any heading, Russian labels (Хост:, Пользователь:, Порт:, Пароль:, Ключ:) and bullet lists are recognized too; a datas entry replaces a config entry of the same name): name, host, user, port, source, auth (\"password\" | \"key\" | \"key+password\") and hasPassword. Passwords, key paths and key material are never returned; a profile with neither an explicit key nor a password still authenticates via ssh-agent (auth \"key\")."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let profiles = match all_profiles(ctx) {
            Ok(p) => p,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let profiles: Vec<Value> = profiles
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "host": p.host,
                    "user": p.user,
                    "port": p.port,
                    "hasPassword": p.password.is_some(),
                    "auth": auth_kind(p),
                    "source": p.source,
                })
            })
            .collect();
        Ok(ToolResult::ok(json!({ "profiles": profiles })))
    }
}

pub struct SshExecTool;

impl Tool for SshExecTool {
    fn name(&self) -> &str {
        "ssh_exec"
    }
    fn description(&self) -> &str {
        "Run a command on a remote host via the system ssh CLI (host keys follow accept-new by default). Target: a named profile from Settings/config/ssh.json or the current project's datas.md (the profile wins over explicit fields and carries its own key/password, so prefer profiles) or explicit host/user/port/keyPath/password. Key-only profiles run with BatchMode=yes (keys/ssh-agent); a profile or explicit password authenticates through SSH_ASKPASS (the secret never appears in argv, results or errors; exactly one prompt). With a key and a password, publickey is tried first. Each call is a fresh non-interactive session: nothing persists between calls, so keep commands self-contained and drive package managers non-interactively (apt-get -y, DEBIAN_FRONTEND=noninteractive, sudo -n). By default the command runs as `bash -lc '<command>'` so PATH/profile apply; set loginShell=false to run it verbatim. Optional cwd runs it inside that remote directory. pty=true allocates a TTY (-tt) for programs that need one, but interactive prompts then hang until the timeout. timeoutSec defaults to 120 and is clamped to 10..1800; long installs should raise it or run detached (nohup ... > log 2>&1 &) and poll the log. Returns exitCode, stdout, stderr; output capped."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "keyPath": { "type": "string" },
                "password": { "type": "string" },
                "command": { "type": "string" },
                "timeoutSec": { "type": "integer" },
                "loginShell": { "type": "boolean" },
                "cwd": { "type": "string" },
                "pty": { "type": "boolean" }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::CheckConditional
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if is_read_only_command(command) {
            return None;
        }
        let label = target_label(args, ctx);
        let mut reason = format!("выполнение команды по SSH на {label}");
        let flags = destructive_reasons(command);
        if !flags.is_empty() {
            reason.push_str(&format!("; деструктивные признаки: {}", flags.join("; ")));
        }
        Some(reason)
    }
    fn max_result_len(&self) -> usize {
        SSH_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args, ctx) {
            Ok(t) => t,
            Err(e) => return Ok(target_error(e)),
        };
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if command.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "command is empty"));
        }
        let opts = ExecOpts {
            login_shell: args
                .get("loginShell")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            cwd: args
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
            pty: args.get("pty").and_then(Value::as_bool).unwrap_or(false),
            timeout: clamp_timeout(args.get("timeoutSec").and_then(Value::as_i64)),
        };
        let argv = ssh_exec_argv(&target, command, &opts);
        let askpass = askpass_for(&target, &wd)?;
        let envs = askpass.as_ref().map(|a| a.envs()).unwrap_or_default();
        let out = run_process("ssh", &argv, &wd, opts.timeout, &envs)?;
        Ok(ToolResult::ok(json!({
            "target": target.label(),
            "exitCode": out.code,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

pub struct SshCopyTool {
    upload: bool,
}

impl SshCopyTool {
    pub fn upload() -> Self {
        Self { upload: true }
    }
    pub fn download() -> Self {
        Self { upload: false }
    }
}

impl Tool for SshCopyTool {
    fn name(&self) -> &str {
        if self.upload {
            "ssh_upload"
        } else {
            "ssh_download"
        }
    }
    fn description(&self) -> &str {
        if self.upload {
            "Copy a local file or folder to the remote host via the system scp CLI (recursive=true adds -r). localPath is project-relative inside the project folder (project agent) or a resolved path (harness agent); remotePath is passed through to scp unchanged. Target and auth resolution is the same as ssh_exec (profiles from ssh.json and the current project's datas.md, key or askpass password auth)."
        } else {
            "Copy a remote file or folder to the local disk via the system scp CLI (recursive=true adds -r). localPath is the landing path (project-contained for the project agent, resolved for the harness agent); remotePath is passed through to scp unchanged. Target and auth resolution is the same as ssh_exec (profiles from ssh.json and the current project's datas.md, key or askpass password auth)."
        }
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "keyPath": { "type": "string" },
                "password": { "type": "string" },
                "localPath": { "type": "string" },
                "remotePath": { "type": "string" },
                "recursive": { "type": "boolean" }
            },
            "required": ["localPath", "remotePath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let label = target_label(args, ctx);
        Some(if self.upload {
            format!("копирование локального файла на SSH-хост {label}")
        } else {
            format!("копирование файла с SSH-хоста {label} на локальный диск")
        })
    }
    fn max_result_len(&self) -> usize {
        SSH_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args, ctx) {
            Ok(t) => t,
            Err(e) => return Ok(target_error(e)),
        };
        let remote = args.get("remotePath").and_then(Value::as_str).unwrap_or("");
        if let Err(e) = check_remote_path(remote) {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string()));
        }
        let local_raw = args.get("localPath").and_then(Value::as_str).unwrap_or("");
        let local = match resolve_local_path(local_raw, ctx, &wd, self.upload) {
            Ok(p) => p,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let recursive = args.get("recursive").and_then(Value::as_bool).unwrap_or(false);
        let (src, dst) = if self.upload {
            (local.display().to_string(), remote.to_string())
        } else {
            (remote.to_string(), local.display().to_string())
        };
        let argv = scp_argv(&target, &src, &dst, recursive);
        let askpass = askpass_for(&target, &wd)?;
        let envs = askpass.as_ref().map(|a| a.envs()).unwrap_or_default();
        let out = run_process("scp", &argv, &wd, SSH_TIMEOUT, &envs)?;
        Ok(ToolResult::ok(json!({
            "target": target.label(),
            "exitCode": out.code,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

// ---- key generation and installation ----

// Where ssh_generate_key puts a key when no path is given: .ssh/ inside the
// current project (project agent) or Harness/tmp/ssh/ (harness agent).
fn default_key_path(ctx: &ToolCtx, wd: &Path) -> Result<PathBuf> {
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            join_rel(&root, ".ssh/id_ed25519")
        }
        None => Ok(paths::harness_tmp_dir(wd).join("ssh").join("id_ed25519")),
    }
}

pub struct SshGenerateKeyTool;

impl Tool for SshGenerateKeyTool {
    fn name(&self) -> &str {
        "ssh_generate_key"
    }
    fn description(&self) -> &str {
        "Generate a new ed25519 SSH keypair with the system ssh-keygen CLI (`ssh-keygen -t ed25519 -N '' -f <path>`, optional -C comment). path is a resolved path (~/wd-relative/absolute); it defaults to .ssh/id_ed25519 inside the current project folder (project agent) or Harness/tmp/ssh/id_ed25519 (harness agent). An existing key file is never overwritten. Returns privateKeyPath and publicKey (the .pub line); install the public key on a host with ssh_install_key and reference the private key as keyPath in profiles or calls."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "comment": { "type": "string" }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let where_ = match (
            args.get("path").and_then(Value::as_str),
            ctx.working_dir.as_deref(),
        ) {
            (Some(raw), Some(wd)) if !raw.trim().is_empty() => shorten(&resolve_path(raw.trim(), wd)),
            (_, Some(wd)) => match default_key_path(ctx, wd) {
                Ok(p) => shorten(&p),
                Err(_) => "папке по умолчанию".to_string(),
            },
            _ => "папке по умолчанию".to_string(),
        };
        Some(format!("генерация SSH-ключа ed25519 в {where_}"))
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("").trim();
        let path = if raw.is_empty() {
            default_key_path(ctx, &wd)?
        } else {
            resolve_path(raw, &wd)
        };
        if path.exists() {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("refusing to overwrite existing key file: {}", shorten(&path)),
            ));
        }
        let comment = args.get("comment").and_then(Value::as_str).unwrap_or("").trim();
        if comment.chars().any(char::is_control) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "comment must not contain control characters",
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut argv = vec![
            "-t".to_string(),
            "ed25519".to_string(),
            "-N".to_string(),
            String::new(),
            "-f".to_string(),
            path.display().to_string(),
        ];
        if !comment.is_empty() {
            argv.push("-C".to_string());
            argv.push(comment.to_string());
        }
        let out = run_process("ssh-keygen", &argv, &wd, Duration::from_secs(30), &[])?;
        if out.code != 0 {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("ssh-keygen failed: {}", out.stderr.trim()),
            ));
        }
        let public = match fs::read_to_string(format!("{}.pub", path.display())) {
            Ok(text) => text.trim().to_string(),
            Err(_) => String::new(),
        };
        Ok(ToolResult::ok(json!({
            "privateKeyPath": path.display().to_string(),
            "publicKey": public,
        })))
    }
}

// The idempotent remote command that appends a public key to
// authorized_keys unless the exact line is already there. The key is
// single-quoted; public keys are not secrets, but the quoting keeps the
// command a single argv element whatever the comment field contains.
fn install_key_command(public_key: &str) -> String {
    let q = shell_quote(public_key);
    format!(
        "mkdir -p ~/.ssh; chmod 700 ~/.ssh; grep -qxF {q} ~/.ssh/authorized_keys || echo {q} >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys"
    )
}

pub struct SshInstallKeyTool;

impl Tool for SshInstallKeyTool {
    fn name(&self) -> &str {
        "ssh_install_key"
    }
    fn description(&self) -> &str {
        "Install a local SSH public key into the remote account's authorized_keys (idempotent): reads <keyPath>.pub and runs `mkdir -p ~/.ssh; chmod 700 ~/.ssh; grep -qxF '<pub>' ~/.ssh/authorized_keys || echo '<pub>' >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys` in one session. Target and auth resolution is the same as ssh_exec: a profile (ssh.json or the current project's datas.md) or explicit host/user/port plus password (askpass) and/or keyPath. Typical flow: ssh_generate_key, then ssh_install_key with the existing password, then use the key in profiles. Returns exitCode and installed=true on success."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "profile": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "password": { "type": "string" },
                "keyPath": { "type": "string" }
            },
            "required": ["keyPath"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let label = target_label(args, ctx);
        Some(format!("установка SSH-ключа в authorized_keys на хосте {label}"))
    }
    fn max_result_len(&self) -> usize {
        SSH_RESULT_CAP
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let target = match resolve_target(args, ctx) {
            Ok(t) => t,
            Err(e) => return Ok(target_error(e)),
        };
        let key_path = args.get("keyPath").and_then(Value::as_str).unwrap_or("").trim();
        if key_path.is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "keyPath is empty"));
        }
        let pub_path = format!("{}.pub", expand_tilde(key_path).display());
        let public = match fs::read_to_string(&pub_path) {
            Ok(text) => text.trim().to_string(),
            Err(_) => {
                return Ok(ToolResult::err(
                    ToolErrorCode::NotFound,
                    format!("public key file not found: {pub_path} (generate a key first)"),
                ))
            }
        };
        if public.is_empty() || public.contains(['\n', '\r']) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "public key must be a single line",
            ));
        }
        let command = install_key_command(&public);
        let opts = ExecOpts {
            login_shell: false,
            cwd: None,
            pty: false,
            timeout: SSH_TIMEOUT,
        };
        let argv = ssh_exec_argv(&target, &command, &opts);
        let askpass = askpass_for(&target, &wd)?;
        let envs = askpass.as_ref().map(|a| a.envs()).unwrap_or_default();
        let out = run_process("ssh", &argv, &wd, SSH_TIMEOUT, &envs)?;
        Ok(ToolResult::ok(json!({
            "target": target.label(),
            "exitCode": out.code,
            "installed": out.code == 0,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "truncated": out.truncated,
        }))
        .capped(self.max_result_len()))
    }
}

// ---- profile persistence in datas.md ----

pub struct SshSaveProfileToDatasTool;

impl Tool for SshSaveProfileToDatasTool {
    fn name(&self) -> &str {
        "ssh_save_profile_to_datas"
    }
    fn description(&self) -> &str {
        "Save (add or update) an SSH profile as a \"## SSH <name>\" section in the current project's datas.md — the user's chosen storage for access data: host:, user:, port:, optional password: and key: lines. An existing section with the same name (any heading case) is replaced in place, otherwise appended; datas.md is created when missing. The password is written to the file but never echoed back by this or any other SSH tool; ssh_list_profiles only reports hasPassword/auth. The saved profile is immediately usable as \"profile\": \"<name>\" by ssh_exec/ssh_upload/ssh_download."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "host": { "type": "string" },
                "user": { "type": "string" },
                "port": { "type": "integer" },
                "password": { "type": "string" },
                "keyPath": { "type": "string" }
            },
            "required": ["name", "host", "user", "port"],
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
        let pid = ctx
            .project_id
            .ok_or_else(|| AppError::Invalid("no project bound to this agent run".into()))?;
        let wd = require_wd(ctx)?;
        let name = args.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if name.is_empty() || name.starts_with('#') || name.contains(['\n', '\r']) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                "name must be a single plain line without '#'",
            ));
        }
        let host = args.get("host").and_then(Value::as_str).unwrap_or("").trim();
        let user = args.get("user").and_then(Value::as_str).unwrap_or("").trim();
        if !valid_host_or_user(host) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("invalid ssh host {host:?}"),
            ));
        }
        if !valid_host_or_user(user) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("invalid ssh user {user:?}"),
            ));
        }
        let port = args.get("port").and_then(Value::as_i64).unwrap_or(0);
        if !(1..=65535).contains(&port) {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!("ssh port {port} is out of range 1-65535"),
            ));
        }
        let mut body = format!("host: {host}\nuser: {user}\nport: {port}");
        if let Some(pw) = args.get("password").and_then(Value::as_str) {
            if !pw.trim().is_empty() {
                body.push_str(&format!("\npassword: {}", pw.trim()));
            }
        }
        if let Some(k) = args.get("keyPath").and_then(Value::as_str) {
            if !k.trim().is_empty() {
                body.push_str(&format!("\nkey: {}", k.trim()));
            }
        }
        let r = with_conn(ctx, |conn| {
            let current = read_datas(conn, &wd, pid)?;
            let title = datas_section_title(&current, &name);
            let (updated, new_md) = upsert_section(&current, &title, &body);
            write_datas(conn, &wd, pid, &new_md).map(|_| updated)
        });
        Ok(match r {
            Ok(updated) => ToolResult::ok(json!({ "name": name, "updated": updated, "source": "datas" })),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::Arc;

    fn harness_ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    // Project mode: a real CRM project under Projects/.
    fn project_env(tag: &str) -> (PathBuf, ToolCtx) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let dto = projects::create_project(&conn, &wd, "proj").unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            project_id: Some(dto.id),
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        (paths::projects_dir(&wd).join("proj"), ctx)
    }

    fn write_ssh_config(wd: &Path, text: &str) {
        fs::create_dir_all(paths::config_dir(wd)).unwrap();
        fs::write(paths::config_dir(wd).join("ssh.json"), text).unwrap();
    }

    fn target(port: u16, key: Option<&str>) -> Target {
        Target {
            host: "example.ru".into(),
            user: "deploy".into(),
            port,
            key_path: key.map(PathBuf::from),
            password: None,
            strict_host_key: "accept-new",
        }
    }

    fn target_pw(port: u16, key: Option<&str>, password: Option<&str>) -> Target {
        let mut t = target(port, key);
        t.password = password.map(str::to_string);
        t
    }

    fn opts(login_shell: bool, cwd: Option<&str>, pty: bool, timeout_secs: Option<i64>) -> ExecOpts {
        ExecOpts {
            login_shell,
            cwd: cwd.map(str::to_string),
            pty,
            timeout: clamp_timeout(timeout_secs),
        }
    }

    #[test]
    fn ssh_exec_argv_default_uses_login_shell_and_120s() {
        let o = opts(true, None, false, None);
        let argv = ssh_exec_argv(&target(2222, Some("/keys/id_ed25519")), "ls -l", &o);
        assert_eq!(
            argv,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-i",
                "/keys/id_ed25519",
                "deploy@example.ru",
                "bash -lc 'ls -l'",
            ]
        );
        assert_eq!(o.timeout, Duration::from_secs(120));
        // the command is the last argument and there is no "--" anywhere
        assert_eq!(argv[argv.len() - 1], "bash -lc 'ls -l'");
        assert_eq!(argv[argv.len() - 2], "deploy@example.ru");
        assert!(!argv.iter().any(|a| a == "--"), "{argv:?}");
        // no key: no -i flag at all
        let plain = ssh_argv(&target(22, None));
        assert!(!plain.iter().any(|a| a == "-i"), "{plain:?}");
    }

    #[test]
    fn login_shell_false_passes_command_verbatim() {
        let o = opts(false, None, false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "echo hi", &o).pop().unwrap(),
            "echo hi"
        );
        // a bare leading-dash command is still wrapped so the remote shell does
        // not read it as its own option
        assert_eq!(ssh_exec_argv(&target(22, None), "-l", &o).pop().unwrap(), "sh -c '-l'");
        // loginShell=true wraps even a leading-dash command in bash -lc
        let o = opts(true, None, false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "-l", &o).pop().unwrap(),
            "bash -lc '-l'"
        );
        // shell_quote escapes embedded single quotes as '\''
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn cwd_is_shell_quoted_and_prepended() {
        let o = opts(true, Some("/var/www/my dir"), false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "ls", &o).pop().unwrap(),
            "cd '/var/www/my dir' && bash -lc 'ls'"
        );
        // an embedded single quote in cwd is escaped as '\''
        let o = opts(false, Some("/it's"), false, None);
        assert_eq!(
            ssh_exec_argv(&target(22, None), "ls", &o).pop().unwrap(),
            "cd '/it'\\''s' && ls"
        );
        // empty cwd is ignored
        let o = opts(false, Some(""), false, None);
        assert_eq!(ssh_exec_argv(&target(22, None), "ls", &o).pop().unwrap(), "ls");
    }

    #[test]
    fn pty_adds_tt_before_the_destination() {
        let o = opts(true, None, true, None);
        let argv = ssh_exec_argv(&target(22, None), "top", &o);
        let tt = argv.iter().position(|a| a == "-tt").expect("-tt present");
        let host = argv.iter().position(|a| a == "deploy@example.ru").unwrap();
        assert!(tt < host, "-tt must precede the destination: {argv:?}");
        assert_eq!(argv[argv.len() - 1], "bash -lc 'top'");
        assert!(!argv.iter().any(|a| a == "--"), "{argv:?}");
        // pty=false: no -tt at all
        let o = opts(true, None, false, None);
        assert!(!ssh_exec_argv(&target(22, None), "top", &o).iter().any(|a| a == "-tt"));
    }

    #[test]
    fn timeout_is_clamped_to_10_1800() {
        assert_eq!(clamp_timeout(None), Duration::from_secs(120));
        assert_eq!(clamp_timeout(Some(120)), Duration::from_secs(120));
        assert_eq!(clamp_timeout(Some(5)), Duration::from_secs(10));
        assert_eq!(clamp_timeout(Some(0)), Duration::from_secs(10));
        assert_eq!(clamp_timeout(Some(-3)), Duration::from_secs(10));
        assert_eq!(clamp_timeout(Some(1800)), Duration::from_secs(1800));
        assert_eq!(clamp_timeout(Some(99999)), Duration::from_secs(1800));
    }

    #[test]
    fn command_stays_last_element_with_no_double_dash() {
        let o = opts(true, Some("/srv/app"), true, Some(600));
        let argv = ssh_exec_argv(&target(22, None), "apt-get install -y nginx", &o);
        assert_eq!(
            argv[argv.len() - 1],
            "cd '/srv/app' && bash -lc 'apt-get install -y nginx'"
        );
        assert!(!argv.iter().any(|a| a == "--"), "{argv:?}");
    }

    #[test]
    fn scp_argv_carries_port_key_recursive_and_path_order() {
        let up = scp_argv(&target(2222, Some("/keys/id")), "/local/a.txt", "/remote/b.txt", true);
        let joined = up.join(" ");
        assert!(joined.contains("-P 2222"), "{joined}");
        assert!(joined.contains("-i /keys/id"), "{joined}");
        assert!(joined.contains(" -r "), "{joined}");
        assert_eq!(up[up.len() - 2], "/local/a.txt");
        assert_eq!(up[up.len() - 1], "/remote/b.txt");
        let down = scp_argv(&target(22, None), "/remote/b.txt", "/local/a.txt", false);
        assert!(!down.iter().any(|a| a == "-r"), "{down:?}");
        assert_eq!(down[down.len() - 2], "/remote/b.txt");
        assert_eq!(down[down.len() - 1], "/local/a.txt");
    }

    #[test]
    fn profile_resolution_wins_and_unknown_profile_lists_known_names() {
        let wd = temp_test_dir("st-profile");
        let key = wd.join("id_ed25519");
        fs::write(&key, "not a real key").unwrap();
        write_ssh_config(
            &wd,
            &json!({ "profiles": [{
                "name": "prod", "host": "example.ru", "user": "deploy",
                "port": 2222, "keyPath": key.display().to_string(),
                "hostKeyPolicy": "strict"
            }] })
            .to_string(),
        );
        // profile wins over explicit fields
        let t = resolve_target(
            &json!({ "profile": "prod", "host": "ignored.ru", "user": "ignored" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.host, "example.ru");
        assert_eq!(t.user, "deploy");
        assert_eq!(t.port, 2222);
        assert_eq!(t.strict_host_key, "yes");
        assert_eq!(t.key_path.as_deref(), Some(key.as_path()));
        assert_eq!(t.password, None);
        // unknown profile: not_found, message lists the known profiles
        let err = resolve_target(&json!({ "profile": "ghost" }), &harness_ctx(&wd)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found") && msg.contains("prod"), "{msg}");
        let r = SshExecTool
            .execute(&json!({ "profile": "ghost", "command": "ls" }), &harness_ctx(&wd))
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn host_key_policy_values_are_mapped_or_rejected() {
        let wd = temp_test_dir("st-hkp");
        write_ssh_config(
            &wd,
            r#"{ "profiles": [
                 { "name": "off", "host": "example.ru", "user": "u", "hostKeyPolicy": "off" },
                 { "name": "bad", "host": "example.ru", "user": "u", "hostKeyPolicy": "trust-me" }
               ] }"#,
        );
        assert_eq!(
            resolve_target(&json!({ "profile": "off" }), &harness_ctx(&wd))
                .unwrap()
                .strict_host_key,
            "no"
        );
        assert_eq!(
            resolve_target(
                &json!({ "host": "example.ru", "user": "u" }),
                &harness_ctx(&wd)
            )
            .unwrap()
            .strict_host_key,
            "accept-new"
        );
        let err = resolve_target(&json!({ "profile": "bad" }), &harness_ctx(&wd)).unwrap_err();
        assert!(err.to_string().contains("hostKeyPolicy"), "{err}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn explicit_target_requires_host_and_user_and_valid_fields() {
        let wd = temp_test_dir("st-explicit");
        let cases = [
            (json!({ "host": "example.ru" }), "host and user"),
            (json!({ "user": "deploy" }), "host and user"),
            (json!({ "host": "bad host", "user": "u" }), "host"),
            (json!({ "host": "example.ru", "user": "bad user" }), "user"),
            (json!({ "host": "-oProxyCommand=x", "user": "u" }), "host"),
            (json!({ "host": "examp@le.ru", "user": "u" }), "host"),
            (json!({ "host": "example.ru", "user": "u", "port": 0 }), "range"),
            (json!({ "host": "example.ru", "user": "u", "port": 70000 }), "range"),
        ];
        for (args, needle) in cases {
            let err = resolve_target(&args, &harness_ctx(&wd)).unwrap_err();
            assert!(err.to_string().contains(needle), "{args}: {err}");
        }
        let t = resolve_target(
            &json!({ "host": "example.ru", "user": "deploy" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.port, 22);
        assert_eq!(t.strict_host_key, "accept-new");
        assert_eq!(t.key_path, None);
        assert_eq!(t.password, None);
        // an explicit ad-hoc password is carried on the target, empty ones dropped
        let t = resolve_target(
            &json!({ "host": "example.ru", "user": "deploy", "password": "hunter2" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.password.as_deref(), Some("hunter2"));
        let t = resolve_target(
            &json!({ "host": "example.ru", "user": "deploy", "password": "" }),
            &harness_ctx(&wd),
        )
        .unwrap();
        assert_eq!(t.password, None);
        // a missing keyPath is a clear not-found error, home shortened to ~
        let err = resolve_target(
            &json!({ "host": "example.ru", "user": "u", "keyPath": "~/definitely-missing-key" }),
            &harness_ctx(&wd),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found") && msg.starts_with("invalid input: ssh key file not found: ~/"), "{msg}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn destructive_labeling_flags_patterns_and_wrappers() {
        let cases = [
            ("rm -rf /tmp/x", true),
            ("rm -fr dir", true),
            ("rm --recursive dir", true),
            ("rm dir/file", false),
            ("sudo rm -rf /srv", true),
            ("timeout 5 dd if=/dev/zero of=/dev/sdb", true),
            ("mkfs.ext4 /dev/sdb1", true),
            ("sudo shutdown -h now", true),
            ("reboot", true),
            ("nohup reboot &", true),
            ("env FOO=1 shutdown", true),
            (":(){ :|:& };:", true),
            ("echo x > /dev/sda", true),
            ("git reset --hard HEAD~1", true),
            ("git -C repo reset --hard", true),
            ("git commit -m x", false),
            ("chmod -R 777 /var/www", true),
            ("chmod 644 f", false),
            ("pkill nginx", true),
            ("killall node", true),
            ("ls -la && echo done", false),
            ("\"rm\" -rf x", true),
            ("echo 'deploy the app'", false),
        ];
        for (cmd, expect) in cases {
            let reasons = destructive_reasons(cmd);
            assert_eq!(!reasons.is_empty(), expect, "{cmd:?} -> {reasons:?}");
            for r in &reasons {
                assert!(!r.is_empty());
            }
        }
        // reasons name the matched pattern
        assert!(destructive_reasons("rm -rf /")[0].contains("rm"));
        assert!(destructive_reasons(":(){ :|:& };:").contains(&"fork-бомба".to_string()));
    }

    #[test]
    fn exec_danger_reason_includes_destructive_analysis() {
        let wd = temp_test_dir("st-danger");
        let ctx = harness_ctx(&wd);
        let r = SshExecTool
            .danger_reason(
                &json!({ "host": "example.ru", "user": "deploy", "command": "sudo rm -rf /srv" }),
                &ctx,
            )
            .unwrap();
        assert!(r.contains("example.ru"), "{r}");
        assert!(r.contains("rm"), "{r}");
        // not on the read-only allowlist: confirms, without destructive flags
        let safe = SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "deploy", "command": "echo hi" }), &ctx)
            .unwrap();
        assert!(!safe.contains("деструктив"), "{safe}");
        // read-only commands never confirm, even with an unresolvable target
        assert!(SshExecTool
            .danger_reason(&json!({ "command": "ls" }), &ToolCtx::default())
            .is_none());
        // unresolvable target still yields a reason for non-read-only commands
        assert!(SshExecTool
            .danger_reason(&json!({ "command": "echo hi" }), &ToolCtx::default())
            .is_some());
        assert!(SshListProfilesTool
            .danger_reason(&json!({}), &ctx)
            .is_none());
        assert!(SshCopyTool::upload()
            .danger_reason(&json!({ "localPath": "a", "remotePath": "b" }), &ctx)
            .is_some());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn ssh_exec_read_only_commands_skip_confirmation() {
        let wd = temp_test_dir("st-readonly");
        let ctx = harness_ctx(&wd);
        let reason = |command: &str| {
            SshExecTool.danger_reason(
                &json!({ "host": "example.ru", "user": "deploy", "command": command }),
                &ctx,
            )
        };
        for safe in [
            "ls",
            "ls -la /var/log",
            "cat /etc/hosts",
            "head -n 5 f",
            "tail -f log",
            "grep -i x f",
            "find . -name x",
            "ps aux",
            "top -bn1",
            "df -h",
            "du -sh .",
            "free -m",
            "uptime",
            "whoami",
            "id",
            "pwd",
            "uname -a",
            "hostname",
            "date",
            "ss -tlnp",
            "netstat -tuln",
            "wc -l f",
            "stat f",
            "file f",
            "git status",
            "git log --oneline -5",
            "git diff HEAD",
            "docker ps -a",
            "systemctl status nginx",
            "ip addr show",
        ] {
            assert!(reason(safe).is_none(), "read-only {safe:?} must not confirm");
        }
        for unsafe_case in [
            "ls | wc -l",
            "ls > out.txt",
            "cat < in.txt",
            "ls && echo done",
            "false || ls",
            "ls &",
            "echo $(whoami)",
            "ls `id`",
            "ls\nrm -rf /",
            "rm -rf /tmp/x",
            "sudo ls",
            "apt-get update",
            "git push",
            "git -C repo status",
            "docker ps | wc -l",
            "ip route",
            "unknowncmd",
            "",
            "   ",
        ] {
            assert!(reason(unsafe_case).is_some(), "{unsafe_case:?} must confirm");
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn local_paths_stay_inside_the_project_in_project_mode() {
        let (root, ctx) = project_env("st-local");
        let wd = ctx.working_dir.clone().unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        assert!(resolve_local_path("sub", &ctx, &wd, true).is_ok());
        for bad in ["../x", "/etc", "a/../../x", ".."] {
            let err = resolve_local_path(bad, &ctx, &wd, false).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("escapes") || msg.contains("not project-relative"),
                "{bad}: {msg}"
            );
        }
        // download destinations may not exist yet, uploads must
        assert!(resolve_local_path("out.txt", &ctx, &wd, false).is_ok());
        assert!(resolve_local_path("out.txt", &ctx, &wd, true)
            .unwrap_err()
            .to_string()
            .contains("not found"));
        // harness mode: absolute paths are accepted
        let hwd = temp_test_dir("st-local-h");
        fs::write(hwd.join("f.txt"), "x").unwrap();
        let abs = hwd.join("f.txt").display().to_string();
        assert_eq!(
            resolve_local_path(&abs, &harness_ctx(&hwd), &hwd, true).unwrap(),
            hwd.join("f.txt").canonicalize().unwrap()
        );
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&hwd).unwrap();
    }

    #[test]
    fn remote_paths_reject_newlines_and_nul_but_pass_through() {
        assert!(check_remote_path("/var/www/app/file.tar.gz").is_ok());
        assert!(check_remote_path("path with spaces").is_ok());
        assert!(check_remote_path("").is_err());
        assert!(check_remote_path("a\nb").is_err());
        assert!(check_remote_path("a\u{0}b").is_err());
        // the tool surfaces it as invalid_args before any local resolution
        let wd = temp_test_dir("st-remote");
        let r = SshCopyTool::upload()
            .execute(
                &json!({ "host": "example.ru", "user": "deploy", "localPath": "missing.txt", "remotePath": "a\nb" }),
                &harness_ctx(&wd),
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        // missing upload source is not_found (and never reaches scp)
        let r = SshCopyTool::upload()
            .execute(
                &json!({ "host": "example.ru", "user": "deploy", "localPath": "missing.txt", "remotePath": "/tmp/x" }),
                &harness_ctx(&wd),
            )
            .unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn output_cap_marks_truncation() {
        let (text, truncated) = read_capped(Cursor::new("x".repeat(30_000)));
        assert_eq!(text.len(), SSH_OUTPUT_CAP);
        assert!(truncated);
        let (text, truncated) = read_capped(Cursor::new("short"));
        assert_eq!(text, "short");
        assert!(!truncated);
    }

    #[test]
    fn list_profiles_never_exposes_key_paths() {
        let wd = temp_test_dir("st-list");
        write_ssh_config(
            &wd,
            r#"{ "profiles": [{ "name": "prod", "host": "example.ru", "user": "deploy", "port": 2222, "keyPath": "/home/deploy/.ssh/id_ed25519" }] }"#,
        );
        let r = SshListProfilesTool
            .execute(&json!({}), &harness_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["profiles"][0]["name"], "prod");
        assert_eq!(data["profiles"][0]["port"], 2222);
        let text = data.to_string();
        assert!(!text.contains("id_ed25519"), "{text}");
        assert!(!text.contains("keyPath"), "{text}");
        // missing ssh.json: empty list, no error
        let empty_wd = temp_test_dir("st-list-empty");
        let r = SshListProfilesTool
            .execute(&json!({}), &harness_ctx(&empty_wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["profiles"], json!([]));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&empty_wd).unwrap();
    }

    #[test]
    fn ssh_config_default_shape_and_unknown_keys_survive() {
        let dir = temp_test_dir("st-config");
        config::ensure_defaults(&dir).unwrap();
        assert_eq!(
            config::read_config(&dir, ConfigKind::Ssh).unwrap(),
            json!({ "profiles": [] })
        );
        config::write_config(
            &dir,
            ConfigKind::Ssh,
            json!({ "profiles": [{
                "name": "prod", "host": "example.ru", "user": "deploy", "port": 22,
                "unknownInProfile": true
            }], "futureTop": true }),
        )
        .unwrap();
        let cfg = load_config(&dir).unwrap();
        assert_eq!(cfg.profiles[0].name, "prod");
        assert_eq!(cfg.profiles[0].port, 22);
        assert_eq!(cfg.extra["futureTop"], true);
        assert_eq!(cfg.profiles[0].extra["unknownInProfile"], true);
        // typed round-trip keeps the unknown keys
        config::write_config(&dir, ConfigKind::Ssh, serde_json::to_value(&cfg).unwrap()).unwrap();
        let raw = config::read_config(&dir, ConfigKind::Ssh).unwrap();
        assert_eq!(raw["futureTop"], true);
        assert_eq!(raw["profiles"][0]["unknownInProfile"], true);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn classes_policies_and_schemas_match_the_spec() {
        assert_eq!(SshListProfilesTool.class(), ToolClass::ReadOnly);
        assert_eq!(SshListProfilesTool.policy(), PermissionPolicy::Skip);
        // ssh_exec is per-call: read-only commands skip, the rest confirm
        assert_eq!(SshExecTool.class(), ToolClass::Mutating);
        assert_eq!(SshExecTool.policy(), PermissionPolicy::CheckConditional);
        assert!(SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "u", "command": "ls" }), &ToolCtx::default())
            .is_none());
        assert!(SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "u", "command": "rm -rf /tmp/x" }), &ToolCtx::default())
            .is_some());
        let mutating: Vec<Box<dyn Tool>> = vec![
            Box::new(SshCopyTool::upload()),
            Box::new(SshCopyTool::download()),
            Box::new(SshGenerateKeyTool),
            Box::new(SshInstallKeyTool),
        ];
        for t in mutating {
            assert_eq!(t.class(), ToolClass::Mutating, "{}", t.name());
            assert_eq!(t.policy(), PermissionPolicy::Check, "{}", t.name());
            assert!(
                t.danger_reason(&json!({ "command": "ls", "localPath": "a", "remotePath": "b" }), &ToolCtx::default())
                    .is_some(),
                "{} danger_reason",
                t.name()
            );
        }
        let exec = SshExecTool.parameters();
        assert!(validate_args(&exec, &json!({ "command": "ls" })).is_ok());
        assert!(validate_args(&exec, &json!({})).unwrap_err().contains("command"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "port": "x" }))
            .unwrap_err()
            .contains("integer"));
        // new ssh_exec options validate against the schema
        assert!(validate_args(
            &exec,
            &json!({ "command": "ls", "timeoutSec": 300, "loginShell": false, "cwd": "/srv", "pty": true })
        )
        .is_ok());
        assert!(validate_args(&exec, &json!({ "command": "ls", "timeoutSec": "x" }))
            .unwrap_err()
            .contains("integer"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "pty": "yes" }))
            .unwrap_err()
            .contains("boolean"));
        assert!(validate_args(&exec, &json!({ "command": "ls", "cwd": 5 }))
            .unwrap_err()
            .contains("string"));
        let copy = SshCopyTool::upload().parameters();
        assert!(validate_args(&copy, &json!({ "localPath": "a", "remotePath": "b" })).is_ok());
        assert!(validate_args(&copy, &json!({ "localPath": "a" }))
            .unwrap_err()
            .contains("remotePath"));
        assert!(validate_args(&copy, &json!({ "localPath": "a", "remotePath": "b", "recursive": "yes" }))
            .unwrap_err()
            .contains("boolean"));
        // password is an accepted ad-hoc auth field on exec and copy
        assert!(validate_args(&exec, &json!({ "command": "ls", "password": "p" })).is_ok());
        assert!(validate_args(&copy, &json!({ "localPath": "a", "remotePath": "b", "password": "p" })).is_ok());
        // key generation: everything optional, extras refused
        let gen = SshGenerateKeyTool.parameters();
        assert!(validate_args(&gen, &json!({})).is_ok());
        assert!(validate_args(&gen, &json!({ "path": "p", "comment": "c" })).is_ok());
        assert!(validate_args(&gen, &json!({ "bogus": 1 })).unwrap_err().contains("bogus"));
        // install: keyPath required, target fields optional
        let inst = SshInstallKeyTool.parameters();
        assert!(validate_args(&inst, &json!({ "keyPath": "k" })).is_ok());
        assert!(validate_args(&inst, &json!({ "profile": "p", "password": "x", "keyPath": "k" })).is_ok());
        assert!(validate_args(&inst, &json!({ "host": "h" })).unwrap_err().contains("keyPath"));
        // save-to-datas: the four core fields are required
        assert_eq!(SshSaveProfileToDatasTool.class(), ToolClass::Mutating);
        assert_eq!(SshSaveProfileToDatasTool.policy(), PermissionPolicy::Skip);
        let save = SshSaveProfileToDatasTool.parameters();
        assert!(validate_args(&save, &json!({ "name": "n", "host": "h", "user": "u", "port": 22 })).is_ok());
        assert!(validate_args(&save, &json!({ "name": "n", "host": "h", "user": "u", "port": 22, "password": "p", "keyPath": "k" })).is_ok());
        assert!(validate_args(&save, &json!({ "name": "n" })).unwrap_err().contains("host"));
        assert!(validate_args(&save, &json!({ "name": "n", "host": "h", "user": "u", "port": "x" }))
            .unwrap_err()
            .contains("integer"));
    }

    #[test]
    fn parse_datas_ssh_reads_valid_sections() {
        let text = "# Project example.ru\n\nplain notes, host: not-a-profile\n\n\
             ## SSH prod\nhost: example.ru\nuser: deploy\nport: 2222\npassword: s3cret\nkey: ~/.ssh/id_ed25519\nextra prose here\n\n\
             ## FTP notes\nhost: ftp.example.ru\nlogin: user-1\n\n\
             ## ssh staging\nHOST: stage.example.ru\nUser: web\nKeyPath: ~/keys/stage\n";
        let ps = parse_datas_ssh(text);
        assert_eq!(ps.len(), 2, "{ps:?}");
        let prod = &ps[0];
        assert_eq!(prod.name, "prod");
        assert_eq!(prod.host, "example.ru");
        assert_eq!(prod.user, "deploy");
        assert_eq!(prod.port, 2222);
        assert_eq!(prod.password.as_deref(), Some("s3cret"));
        assert_eq!(prod.key_path.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(prod.source, "datas");
        let staging = &ps[1];
        assert_eq!(staging.name, "staging");
        assert_eq!(staging.host, "stage.example.ru");
        assert_eq!(staging.port, 22);
        assert_eq!(staging.password, None);
        assert_eq!(staging.key_path.as_deref(), Some("~/keys/stage"));
    }

    #[test]
    fn parse_datas_ssh_accepts_russian_freeform_sections() {
        let text = "# notes\n\n## Доступ к серверу\n\n- Хост: 192.0.2.10\n- Пользователь: ivan\n- Порт: 2222\n- Пароль: secret-1!\n- Ключ: ~/.ssh/id_ed25519\n- SSH: `ssh ivan@192.0.2.10` (порт по умолчанию 22)\n\n## Notes\njust prose, no fields\n";
        let ps = parse_datas_ssh(text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        let p = &ps[0];
        assert_eq!(p.name, "Доступ к серверу");
        assert_eq!(p.host, "192.0.2.10");
        assert_eq!(p.user, "ivan");
        assert_eq!(p.port, 2222);
        assert_eq!(p.password.as_deref(), Some("secret-1!"));
        assert_eq!(p.key_path.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(p.source, "datas");
    }

    #[test]
    fn parse_datas_ssh_skips_malformed_entries() {
        let text = "## SSH nohost\nuser: u\n\n\
             ## SSH nokey\nhost: example.ru\n\n\
             ## SSH badport\nhost: example.ru\nuser: u\nport: abc\n\n\
             ## SSH bigport\nhost: example.ru\nuser: u\nport: 99999\n\n\
             ## SSH\nhost: example.ru\nuser: u\n\n\
             ## SSH badhost\nhost: bad host\nuser: u\n\n\
             ## SSH empty\n\n\
             ## SSH ok\nhost: example.ru\nuser: u\n";
        let ps = parse_datas_ssh(text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].name, "ok");
        // empty text: no profiles
        assert!(parse_datas_ssh("").is_empty());
    }

    #[test]
    fn list_profiles_merges_config_and_datas_and_never_leaks_secrets() {
        let (root, ctx) = project_env("st-merge");
        let wd = ctx.working_dir.clone().unwrap();
        write_ssh_config(
            &wd,
            &json!({ "profiles": [
                { "name": "web", "host": "web.example.ru", "user": "deploy", "port": 22, "keyPath": "/keys/web" },
                { "name": "cfg", "host": "cfg.example.ru", "user": "u", "port": 2222 }
            ] })
            .to_string(),
        );
        fs::write(
            root.join(paths::DATAS_MD),
            "## SSH db\nhost: db.example.ru\nuser: admin\nport: 33\npassword: hunter2\n\n## SSH web\nhost: web2.example.ru\nuser: admin\npassword: datas-pw\n",
        )
        .unwrap();
        let r = SshListProfilesTool.execute(&json!({}), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let text = data.to_string();
        assert!(!text.contains("hunter2") && !text.contains("datas-pw"), "{text}");
        assert!(!text.contains("keyPath") && !text.contains("/keys/web"), "{text}");
        let by_name = |name: &str| -> Value {
            data["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .cloned()
                .unwrap_or(Value::Null)
        };
        // a datas entry replaces the config entry of the same name
        let web = by_name("web");
        assert_eq!(web["source"], "datas");
        assert_eq!(web["host"], "web2.example.ru");
        assert_eq!(web["hasPassword"], true);
        assert_eq!(web["auth"], "password");
        let dupes = data["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["name"] == "web")
            .count();
        assert_eq!(dupes, 1, "{text}");
        let db = by_name("db");
        assert_eq!(db["source"], "datas");
        assert_eq!(db["port"], 33);
        assert_eq!(db["auth"], "password");
        let cfg = by_name("cfg");
        assert_eq!(cfg["source"], "config");
        assert_eq!(cfg["hasPassword"], false);
        // no explicit key/password: still key auth via ssh-agent
        assert_eq!(cfg["auth"], "key");
        // resolution from datas carries the password and key invisibly
        let t = resolve_target(&json!({ "profile": "db" }), &ctx).unwrap();
        assert_eq!(t.password.as_deref(), Some("hunter2"));
        assert_eq!(t.port, 33);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn password_auth_switches_ssh_options_and_keeps_the_secret_out_of_argv() {
        let o = opts(true, None, false, None);
        // password only: BatchMode=no, exactly one prompt, no secret in argv
        let argv = ssh_exec_argv(&target_pw(22, None, Some("hunter2")), "ls", &o);
        let joined = argv.join(" ");
        assert!(joined.contains("BatchMode=no"), "{joined}");
        assert!(joined.contains("NumberOfPasswordPrompts=1"), "{joined}");
        assert!(!joined.contains("BatchMode=yes"), "{joined}");
        assert!(!joined.contains("hunter2"), "{joined}");
        // key + password: -i stays, publickey is tried before the password
        let argv = ssh_exec_argv(&target_pw(2222, Some("/keys/id"), Some("hunter2")), "ls", &o);
        let joined = argv.join(" ");
        assert!(joined.contains("-i /keys/id"), "{joined}");
        assert!(
            joined.contains("PreferredAuthentications=publickey,password,keyboard-interactive"),
            "{joined}"
        );
        assert!(joined.contains("BatchMode=no"), "{joined}");
        assert!(!joined.contains("hunter2"), "{joined}");
        // key only: BatchMode=yes, no prompt plumbing at all
        let joined = ssh_exec_argv(&target(22, Some("/keys/id")), "ls", &o).join(" ");
        assert!(joined.contains("BatchMode=yes"), "{joined}");
        assert!(!joined.contains("NumberOfPasswordPrompts"), "{joined}");
        assert!(!joined.contains("PreferredAuthentications"), "{joined}");
        // scp gets the same option set through common_opts
        let sp = scp_argv(&target_pw(2222, None, Some("hunter2")), "a", "b", false);
        let joined = sp.join(" ");
        assert!(joined.contains("BatchMode=no"), "{joined}");
        assert!(!joined.contains("hunter2"), "{joined}");
    }

    #[test]
    fn askpass_files_are_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let wd = temp_test_dir("st-askpass");
        let a = Askpass::create(&wd, "hunter2").unwrap();
        assert!(a.dir.starts_with(paths::harness_tmp_dir(&wd)), "{:?}", a.dir);
        let pass = a.dir.join("pass");
        assert_eq!(fs::metadata(&a.dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&pass).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&a.script).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::read_to_string(&pass).unwrap(), "hunter2");
        // the script itself never embeds the secret, it cats the 0600 file
        let script_text = fs::read_to_string(&a.script).unwrap();
        assert!(!script_text.contains("hunter2"), "{script_text}");
        let out = Command::new(&a.script).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hunter2");
        let envs = a.envs();
        let get = |k: &str| {
            envs.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("SSH_ASKPASS_REQUIRE").as_deref(), Some("force"));
        assert_eq!(get("DISPLAY").as_deref(), Some(":0"));
        assert_eq!(get("SSH_ASKPASS").as_deref(), a.script.to_str());
        // success or failure alike: Drop removes the whole private dir
        let dir = a.dir.clone();
        drop(a);
        assert!(!dir.exists() && !pass.exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    // Backdate a path's mtime (futimens through a read-only dir fd works
    // on macOS; the ops tmp-cleanup tests use the same trick for files).
    fn backdate(path: &Path, age: Duration) {
        let file = fs::File::open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn purge_stale_askpass_dirs_removes_only_stale_leftovers() {
        let wd = temp_test_dir("st-askpass-purge");
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp).unwrap();
        let two_hours = Duration::from_secs(7200);

        // stale: dead pid + old mtime, with a nested password file
        let stale = tmp.join("ssh-askpass-999999-1");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("pass"), "hunter2").unwrap();
        backdate(&stale, two_hours);

        // live pid: our own process, even with an old mtime
        let live = tmp.join(format!("ssh-askpass-{}-2", std::process::id()));
        fs::create_dir(&live).unwrap();
        backdate(&live, two_hours);

        // young: dead pid but created just now (a run of another instance)
        let young = tmp.join("ssh-askpass-999998-3");
        fs::create_dir(&young).unwrap();

        // not our naming scheme: malformed names and foreign entries survive
        let malformed = tmp.join("ssh-askpass-notapid-4");
        fs::create_dir(&malformed).unwrap();
        backdate(&malformed, two_hours);
        let no_nonce = tmp.join("ssh-askpass-999997");
        fs::create_dir(&no_nonce).unwrap();
        backdate(&no_nonce, two_hours);
        fs::write(tmp.join("other.txt"), "keep").unwrap();

        // a same-named stale dir outside Harness/tmp is never touched
        let outside_tmp = paths::harness_dir(&wd).join("ssh-askpass-999996-5");
        fs::create_dir(&outside_tmp).unwrap();
        backdate(&outside_tmp, two_hours);

        assert_eq!(purge_stale_askpass_dirs(&wd), 1);
        assert!(!stale.exists());
        assert!(live.exists(), "live-pid dir was purged");
        assert!(young.exists(), "young dir was purged");
        assert!(malformed.exists() && no_nonce.exists());
        assert!(tmp.join("other.txt").is_file());
        assert!(outside_tmp.is_dir(), "purge reached outside Harness/tmp");
        // idempotent: nothing left to remove
        assert_eq!(purge_stale_askpass_dirs(&wd), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn purge_stale_askpass_dirs_never_follows_symlinks_out_of_tmp() {
        use std::os::unix::fs::symlink;
        let wd = temp_test_dir("st-askpass-purge-link");
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp).unwrap();
        let two_hours = Duration::from_secs(7200);

        // outside Harness/tmp: must survive in both cases
        let outside = wd.join("outside");
        fs::create_dir_all(outside.join("secret")).unwrap();
        let leak = outside.join("secret/leak.txt");
        fs::write(&leak, "leak").unwrap();

        // a stale dir whose inside entry is a symlink out of tmp
        let stale = tmp.join("ssh-askpass-999999-1");
        fs::create_dir(&stale).unwrap();
        symlink(&outside, stale.join("link")).unwrap();
        backdate(&stale, two_hours);

        // a matching symlink entry itself: unlinked, target untouched
        let link = tmp.join("ssh-askpass-999998-2");
        symlink(&outside, &link).unwrap();

        assert_eq!(purge_stale_askpass_dirs(&wd), 2);
        assert!(!stale.exists() && !link.exists());
        assert!(leak.is_file(), "the purge followed a symlink out of Harness/tmp");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn install_key_command_is_idempotent_and_quoted() {
        let cmd = install_key_command("ssh-ed25519 AAAAC3NzaC1lZDI11 key comment");
        assert!(cmd.contains("mkdir -p ~/.ssh"), "{cmd}");
        assert!(cmd.contains("chmod 700 ~/.ssh"), "{cmd}");
        assert!(
            cmd.contains("grep -qxF 'ssh-ed25519 AAAAC3NzaC1lZDI11 key comment' ~/.ssh/authorized_keys"),
            "{cmd}"
        );
        assert!(
            cmd.contains("|| echo 'ssh-ed25519 AAAAC3NzaC1lZDI11 key comment' >> ~/.ssh/authorized_keys"),
            "{cmd}"
        );
        assert!(cmd.ends_with("chmod 600 ~/.ssh/authorized_keys"), "{cmd}");
        // an embedded single quote in the key comment is shell-escaped
        let cmd = install_key_command("ssh-ed25519 AAA it's");
        assert!(cmd.contains("'ssh-ed25519 AAA it'\\''s'"), "{cmd}");
    }

    #[test]
    fn generate_key_creates_a_real_keypair_and_refuses_overwrite() {
        let wd = temp_test_dir("st-keygen");
        let ctx = harness_ctx(&wd);
        let path = wd.join("keys/id_ed25519");
        let r = SshGenerateKeyTool
            .execute(
                &json!({ "path": path.display().to_string(), "comment": "test@s" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let public = data["publicKey"].as_str().unwrap();
        assert!(public.starts_with("ssh-ed25519 "), "{public}");
        assert!(public.contains("test@s"), "{public}");
        assert!(path.is_file());
        assert!(Path::new(&format!("{}.pub", path.display())).is_file());
        assert_eq!(data["privateKeyPath"], path.display().to_string());
        // an existing key file is never overwritten
        let r = SshGenerateKeyTool
            .execute(&json!({ "path": path.display().to_string() }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        // harness default: Harness/tmp/ssh/id_ed25519
        let r = SshGenerateKeyTool.execute(&json!({}), &ctx).unwrap();
        let p = r.data.unwrap()["privateKeyPath"].as_str().unwrap().to_string();
        assert!(
            p.starts_with(&paths::harness_tmp_dir(&wd).join("ssh").display().to_string()),
            "{p}"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn generate_key_defaults_inside_the_project_folder() {
        let (root, ctx) = project_env("st-keygen-p");
        let wd = ctx.working_dir.clone().unwrap();
        let r = SshGenerateKeyTool.execute(&json!({}), &ctx).unwrap();
        assert!(r.ok, "{:?}", r.error);
        let p = r.data.unwrap()["privateKeyPath"].as_str().unwrap().to_string();
        // project_root is canonicalized (/private/var on macOS), the raw
        // project_env root is not: compare through canonicalize().
        assert_eq!(
            p,
            root.canonicalize().unwrap().join(".ssh/id_ed25519").display().to_string()
        );
        assert!(root.join(".ssh/id_ed25519").is_file());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn save_profile_to_datas_appends_and_updates_in_place() {
        let (root, ctx) = project_env("st-save");
        let wd = ctx.working_dir.clone().unwrap();
        let datas = root.join(paths::DATAS_MD);
        let r = SshSaveProfileToDatasTool
            .execute(
                &json!({ "name": "prod", "host": "example.ru", "user": "deploy", "port": 2222, "password": "hunter2", "keyPath": "~/.ssh/id_ed25519" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["updated"], false);
        // the result never echoes the password
        assert!(!r.data.unwrap().to_string().contains("hunter2"));
        let text = fs::read_to_string(&datas).unwrap();
        assert!(text.contains("## SSH prod"), "{text}");
        assert!(text.contains("password: hunter2"), "{text}");
        assert!(text.contains("key: ~/.ssh/id_ed25519"), "{text}");
        // update replaces the section in place, the old password is gone
        let r = SshSaveProfileToDatasTool
            .execute(
                &json!({ "name": "prod", "host": "example.ru", "user": "deploy", "port": 22, "password": "newpass" }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.data.unwrap()["updated"], true);
        let text = fs::read_to_string(&datas).unwrap();
        assert_eq!(text.matches("## SSH prod").count(), 1, "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        let ps = parse_datas_ssh(&text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].password.as_deref(), Some("newpass"));
        assert_eq!(ps[0].port, 22);
        assert_eq!(ps[0].key_path, None);
        // a hand-written lowercase heading is updated in place, not duplicated
        fs::write(&datas, "## ssh legacy\nhost: old.ru\nuser: old\nport: 22\n").unwrap();
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "legacy", "host": "new.ru", "user": "u", "port": 22 }), &ctx)
            .unwrap();
        assert_eq!(r.data.unwrap()["updated"], true);
        let text = fs::read_to_string(&datas).unwrap();
        assert!(text.contains("## ssh legacy"), "{text}");
        let ps = parse_datas_ssh(&text);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].host, "new.ru");
        // missing datas.md is created
        fs::remove_file(&datas).unwrap();
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "fresh", "host": "f.example.ru", "user": "u", "port": 22 }), &ctx)
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let ps = parse_datas_ssh(&fs::read_to_string(&datas).unwrap());
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].name, "fresh");
        // bad args are refused
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "#x", "host": "example.ru", "user": "u", "port": 22 }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        let r = SshSaveProfileToDatasTool
            .execute(&json!({ "name": "n", "host": "example.ru", "user": "u", "port": 0 }), &ctx)
            .unwrap();
        assert_eq!(r.error.unwrap().code, "invalid_args");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn secrets_never_reach_danger_reasons_or_masked_output() {
        let (root, ctx) = project_env("st-mask");
        let wd = ctx.working_dir.clone().unwrap();
        fs::write(
            root.join(paths::DATAS_MD),
            "## SSH db\nhost: db.example.ru\nuser: admin\npassword: hunter2\n",
        )
        .unwrap();
        let reason = SshExecTool
            .danger_reason(&json!({ "profile": "db", "command": "apt-get update" }), &ctx)
            .unwrap();
        assert!(reason.contains("db.example.ru"), "{reason}");
        assert!(!reason.contains("hunter2"), "{reason}");
        let reason = SshInstallKeyTool
            .danger_reason(&json!({ "profile": "db", "keyPath": "/keys/id" }), &ctx)
            .unwrap();
        assert!(!reason.contains("hunter2"), "{reason}");
        // the journal masking covers datas.md-style password lines
        assert_eq!(mask_secrets("password: hunter2"), "password: ***");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    #[ignore = "live ssh: needs a reachable sshd and a working key/ssh-agent"]
    fn live_ssh_exec_localhost() {
        let wd = temp_test_dir("st-live");
        let user = std::env::var("USER").unwrap_or_default();
        let r = SshExecTool
            .execute(
                &json!({ "host": "localhost", "user": user, "command": "echo hi" }),
                &harness_ctx(&wd),
            )
            .unwrap();
        let data = r.data.expect("live run must return data");
        assert_eq!(data["stdout"].as_str().unwrap().trim(), "hi");
        fs::remove_dir_all(&wd).unwrap();
    }

    // Full live path over a hand-written Russian datas.md section: profile
    // discovery, askpass password auth, ssh_exec and an scp upload/download
    // round-trip under /tmp with remote cleanup. Credentials come from the
    // environment only: SSH_TEST_HOST, SSH_TEST_USER, SSH_TEST_PASSWORD and
    // the optional SSH_TEST_PORT (default 22).
    #[test]
    #[ignore = "live ssh: set SSH_TEST_HOST/SSH_TEST_USER/SSH_TEST_PASSWORD (optional SSH_TEST_PORT)"]
    fn live_ssh_profile_exec_and_scp_roundtrip() {
        let host = std::env::var("SSH_TEST_HOST").expect("SSH_TEST_HOST");
        let user = std::env::var("SSH_TEST_USER").expect("SSH_TEST_USER");
        let password = std::env::var("SSH_TEST_PASSWORD").expect("SSH_TEST_PASSWORD");
        let port: u16 = std::env::var("SSH_TEST_PORT")
            .ok()
            .map(|p| p.parse().expect("SSH_TEST_PORT must be a number"))
            .unwrap_or(22);
        let (root, ctx) = project_env("st-live-profile");
        let wd = ctx.working_dir.clone().unwrap();
        fs::write(
            root.join(paths::DATAS_MD),
            format!(
                "## Доступ к серверу\n\n- Хост: {host}\n- Пользователь: {user}\n- Порт: {port}\n- Пароль: {password}\n- SSH: `ssh {user}@{host}`\n"
            ),
        )
        .unwrap();
        // the hand-written section is discovered as a password profile
        let ps = datas_profiles(&ctx);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].name, "Доступ к серверу");
        assert_eq!(auth_kind(&ps[0]), "password");
        // ssh_exec through the profile: askpass auth, login shell
        let r = SshExecTool
            .execute(
                &json!({ "profile": "Доступ к серверу", "command": "echo SSH_OK; uname -a; whoami; df -h / | tail -1" }),
                &ctx,
            )
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        let stdout = data["stdout"].as_str().unwrap();
        assert!(stdout.contains("SSH_OK"), "{stdout}");
        assert!(!data.to_string().contains(&password), "secret leaked in result");
        // scp round-trip under /tmp, then remote cleanup
        let marker = format!("project-s-live-{}", std::process::id());
        fs::write(root.join("live-upload.txt"), &marker).unwrap();
        let remote = format!("/tmp/{marker}.txt");
        let r = SshCopyTool::upload()
            .execute(
                &json!({ "profile": "Доступ к серверу", "localPath": "live-upload.txt", "remotePath": remote }),
                &ctx,
            )
            .unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        let r = SshCopyTool::download()
            .execute(
                &json!({ "profile": "Доступ к серверу", "localPath": "live-readback.txt", "remotePath": remote }),
                &ctx,
            )
            .unwrap();
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        assert_eq!(fs::read_to_string(root.join("live-readback.txt")).unwrap(), marker);
        let r = SshExecTool
            .execute(
                &json!({ "profile": "Доступ к серверу", "command": format!("rm -f {remote}") }),
                &ctx,
            )
            .unwrap();
        assert_eq!(r.data.unwrap()["exitCode"], 0);
        fs::remove_dir_all(&wd).unwrap();
    }
}
