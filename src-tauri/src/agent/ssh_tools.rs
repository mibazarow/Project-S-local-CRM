// Agent SSH tools via the system ssh/scp CLI with keys/ssh-agent (no
// passwords, no new crates). Profiles live in Settings/config/ssh.json;
// ssh_exec/ssh_upload/ssh_download are Mutating and always confirmed — the
// destructive-command analysis below only enriches the confirmation text,
// the Check itself is the guard. Local paths stay inside the project root
// (project mode) or resolve like the harness fs tools (harness mode);
// remote paths pass through with newline/NUL rejection. Child processes
// see TMPDIR/TEMP/TMP pointed at Harness/tmp/ (SPEC 5.2). Output is
// secret-masked and capped; home paths are shortened to ~ in errors.
use super::git_tools::join_rel;
use super::harness_tools::{canonicalish, resolve_path};
use super::project_tools::{envelope, require_wd, with_conn};
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
use std::time::{Duration, Instant};

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
// port, keyPath, hostKeyPolicy } ] }. Unknown keys (top-level and
// per-profile) survive typed round-trips like the other configs (SPEC 3.4).
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn load_config(wd: &Path) -> Result<SshConfig> {
    let value = config::read_config(wd, ConfigKind::Ssh)?;
    serde_json::from_value(value)
        .map_err(|e| AppError::Invalid(format!("Settings/config/ssh.json is invalid: {e}")))
}

// The resolved endpoint of one tool call.
#[derive(Debug, Clone)]
struct Target {
    host: String,
    user: String,
    port: u16,
    key_path: Option<PathBuf>,
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
        strict_host_key,
    })
}

// Either a named profile from ssh.json (the profile wins over explicit
// fields) or explicit host/user/port/keyPath. host+user are required.
fn resolve_target(args: &Value, wd: &Path) -> Result<Target> {
    let profile = args.get("profile").and_then(Value::as_str).unwrap_or("").trim();
    if !profile.is_empty() {
        let cfg = load_config(wd)?;
        let p = cfg.profiles.iter().find(|p| p.name == profile).ok_or_else(|| {
            let names: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
            let known = if names.is_empty() {
                "none".to_string()
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
    build_target(host, user, port, key_path, None)
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
    ctx.working_dir
        .as_deref()
        .and_then(|wd| resolve_target(args, wd).ok())
        .map(|t| t.label())
        .unwrap_or_else(|| "цель не определена".to_string())
}

// BatchMode=yes: ssh/scp must never fall back to a password prompt; keys
// and ssh-agent are the only authentication paths.
fn common_opts(t: &Target) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("StrictHostKeyChecking={}", t.strict_host_key),
        "-o".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
    ]
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

// `ssh`/`scp` with piped output, TMPDIR/TEMP/TMP pointed at Harness/tmp/,
// a hard timeout (kill) and masked output.
fn run_process(program: &str, argv: &[String], wd: &Path, timeout: Duration) -> Result<SshOutput> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let mut child = match Command::new(program)
        .args(argv)
        .stdin(Stdio::null())
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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

// ---- destructive-command labeling (pure, never blocks) ----

// The Check is the guard; this only explains WHY a command looks dangerous
// in the confirmation queue. It errs on the side of flagging: a false
// positive costs nothing extra (ssh_exec always confirms anyway).
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
        "List SSH profiles from Settings/config/ssh.json: name, host, user and port only; key paths and key material are never returned. Authentication uses keys/ssh-agent (BatchMode), passwords are never supported."
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
        let wd = require_wd(ctx)?;
        let cfg = match load_config(&wd) {
            Ok(c) => c,
            Err(e) => return Ok(envelope::<()>(Err(e))),
        };
        let profiles: Vec<Value> = cfg
            .profiles
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "host": p.host,
                    "user": p.user,
                    "port": p.port,
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
        "Run a command on a remote host via the system ssh CLI (BatchMode: keys/ssh-agent only, never prompts for a password; host keys follow accept-new by default). Target: a named profile from Settings/config/ssh.json (the profile wins over explicit fields) or explicit host/user/port/keyPath. Each call is a fresh non-interactive session: nothing persists between calls, so keep commands self-contained and drive package managers non-interactively (apt-get -y, DEBIAN_FRONTEND=noninteractive, sudo -n). By default the command runs as `bash -lc '<command>'` so PATH/profile apply; set loginShell=false to run it verbatim. Optional cwd runs it inside that remote directory. pty=true allocates a TTY (-tt) for programs that need one, but interactive prompts then hang until the timeout. timeoutSec defaults to 120 and is clamped to 10..1800; long installs should raise it or run detached (nohup ... > log 2>&1 &) and poll the log. Returns exitCode, stdout, stderr; output capped."
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
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let label = target_label(args, ctx);
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
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
        let target = match resolve_target(args, &wd) {
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
        let out = run_process("ssh", &argv, &wd, opts.timeout)?;
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
            "Copy a local file or folder to the remote host via the system scp CLI (recursive=true adds -r). localPath is project-relative inside the project folder (project agent) or a resolved path (harness agent); remotePath is passed through to scp unchanged. Target resolution is the same as ssh_exec."
        } else {
            "Copy a remote file or folder to the local disk via the system scp CLI (recursive=true adds -r). localPath is the landing path (project-contained for the project agent, resolved for the harness agent); remotePath is passed through to scp unchanged. Target resolution is the same as ssh_exec."
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
        let target = match resolve_target(args, &wd) {
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
        let out = run_process("scp", &argv, &wd, SSH_TIMEOUT)?;
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
            strict_host_key: "accept-new",
        }
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
            &wd,
        )
        .unwrap();
        assert_eq!(t.host, "example.ru");
        assert_eq!(t.user, "deploy");
        assert_eq!(t.port, 2222);
        assert_eq!(t.strict_host_key, "yes");
        assert_eq!(t.key_path.as_deref(), Some(key.as_path()));
        // unknown profile: not_found, message lists the known profiles
        let err = resolve_target(&json!({ "profile": "ghost" }), &wd).unwrap_err();
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
            resolve_target(&json!({ "profile": "off" }), &wd)
                .unwrap()
                .strict_host_key,
            "no"
        );
        assert_eq!(
            resolve_target(&json!({ "host": "example.ru", "user": "u" }), &wd)
                .unwrap()
                .strict_host_key,
            "accept-new"
        );
        let err = resolve_target(&json!({ "profile": "bad" }), &wd).unwrap_err();
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
            let err = resolve_target(&args, &wd).unwrap_err();
            assert!(err.to_string().contains(needle), "{args}: {err}");
        }
        let t = resolve_target(&json!({ "host": "example.ru", "user": "deploy" }), &wd).unwrap();
        assert_eq!(t.port, 22);
        assert_eq!(t.strict_host_key, "accept-new");
        assert_eq!(t.key_path, None);
        // a missing keyPath is a clear not-found error, home shortened to ~
        let err = resolve_target(
            &json!({ "host": "example.ru", "user": "u", "keyPath": "~/definitely-missing-key" }),
            &wd,
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
        let safe = SshExecTool
            .danger_reason(&json!({ "host": "example.ru", "user": "deploy", "command": "ls" }), &ctx)
            .unwrap();
        assert!(!safe.contains("деструктив"), "{safe}");
        // unresolvable target still yields a reason (Check always confirms)
        assert!(SshExecTool
            .danger_reason(&json!({ "command": "ls" }), &ToolCtx::default())
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
        let mutating: Vec<Box<dyn Tool>> = vec![
            Box::new(SshExecTool),
            Box::new(SshCopyTool::upload()),
            Box::new(SshCopyTool::download()),
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
}
