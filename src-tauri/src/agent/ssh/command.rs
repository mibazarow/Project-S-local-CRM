// SSH command/argv construction and validation: the resolved Target endpoint,
// host/user/port/key validation, the ssh/scp argv builders (argv only, never
// shell strings), the remote command wrapping, and the local/remote path
// checks the copy tools rely on. Passwords never enter argv here.
use super::config::all_profiles;
use crate::agent::harness_tools::{canonicalish, resolve_path};
use crate::agent::project_tools::with_conn;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::ToolCtx;
use crate::error::{AppError, Result};
use crate::path_policy::join_rel;
use crate::{paths, projects};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const SSH_TIMEOUT: Duration = Duration::from_secs(120);
// ssh_exec timeoutSec bounds: long installs need more than the default 120 s,
// but the call is still capped so a hung session cannot wedge the agent loop.
const SSH_TIMEOUT_MIN_SECS: u64 = 10;
const SSH_TIMEOUT_MAX_SECS: u64 = 1800;
const CONNECT_TIMEOUT_SECS: u32 = 10;
pub(super) const DEFAULT_PORT: u16 = 22;

// The resolved endpoint of one tool call. The password (when any) is only
// ever written to the askpass file: it never enters argv or results.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub(super) host: String,
    pub(super) user: String,
    pub(super) port: u16,
    pub(super) key_path: Option<PathBuf>,
    pub(super) password: Option<String>,
    pub(super) strict_host_key: &'static str,
}

impl Target {
    pub(super) fn label(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }
}

// Hosts and users land in one ssh argv element (user@host): whitespace,
// control characters, '@' and a leading '-' (option injection) are refused.
pub(super) fn valid_host_or_user(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains('@')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn home_dir() -> PathBuf {
    crate::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub(super) fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(raw)
}

// Key paths are not secrets, but the user's home is theirs: show ~ instead.
pub(super) fn shorten(path: &Path) -> String {
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
pub(super) fn resolve_target(args: &Value, ctx: &ToolCtx) -> Result<Target> {
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
pub(super) fn target_error(e: AppError) -> ToolResult {
    let msg = e.to_string();
    if msg.contains("not found") {
        ToolResult::err(ToolErrorCode::NotFound, msg)
    } else {
        ToolResult::err(ToolErrorCode::InvalidArgs, msg)
    }
}

pub(super) fn target_label(args: &Value, ctx: &ToolCtx) -> String {
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

pub(super) fn ssh_argv(t: &Target) -> Vec<String> {
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
pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// Per-call ssh_exec options (defaults resolved in execute()).
pub(super) struct ExecOpts {
    pub(super) login_shell: bool,
    pub(super) cwd: Option<String>,
    pub(super) pty: bool,
    pub(super) timeout: Duration,
}

// timeoutSec default 120 s, clamped to 10..1800 so a hung session cannot
// wedge the agent loop but long installs still get room.
pub(super) fn clamp_timeout(value: Option<i64>) -> Duration {
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
pub(super) fn ssh_exec_argv(t: &Target, command: &str, opts: &ExecOpts) -> Vec<String> {
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

pub(super) fn scp_argv(t: &Target, src: &str, dst: &str, recursive: bool) -> Vec<String> {
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

// Local paths: project mode keeps every path inside the project root
// (upload targets must exist, download destinations may be new); harness
// mode resolves absolute/~/wd-relative paths like the harness fs tools.
pub(super) fn resolve_local_path(raw: &str, ctx: &ToolCtx, wd: &Path, must_exist: bool) -> Result<PathBuf> {
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
pub(super) fn check_remote_path(path: &str) -> Result<()> {
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

// Where ssh_generate_key puts a key when no path is given: .ssh/ inside the
// current project (project agent) or Harness/tmp/ssh/ (harness agent).
pub(super) fn default_key_path(ctx: &ToolCtx, wd: &Path) -> Result<PathBuf> {
    match ctx.project_id {
        Some(pid) => {
            let root = with_conn(ctx, |conn| projects::project_root(conn, wd, pid))?;
            join_rel(&root, ".ssh/id_ed25519")
        }
        None => Ok(paths::harness_tmp_dir(wd).join("ssh").join("id_ed25519")),
    }
}

// The idempotent remote command that appends a public key to
// authorized_keys unless the exact line is already there. The key is
// single-quoted; public keys are not secrets, but the quoting keeps the
// command a single argv element whatever the comment field contains.
pub(super) fn install_key_command(public_key: &str) -> String {
    let q = shell_quote(public_key);
    format!(
        "mkdir -p ~/.ssh; chmod 700 ~/.ssh; grep -qxF {q} ~/.ssh/authorized_keys || echo {q} >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys"
    )
}
