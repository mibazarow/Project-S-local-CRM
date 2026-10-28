// OS-specific process control for the background-process domain
// (crate::processes) and for the foreground `bash` tool: how a command is
// built, how a process or process group is probed for liveness, and how it is
// signalled. The macOS behavior is unchanged; Windows gets the native
// equivalents, because it has neither POSIX process groups nor POSIX signals.
//
// Deliberately not here: the agent feature tools that happen to be
// macOS-only (clipboard, OCR, PDF, Spotlight, screenshot, open_with,
// browser/CDP). They are product features, not shared infrastructure, so
// their platform specifics stay with the feature.
use std::process::{Child, Command, Stdio};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(any(windows, target_os = "macos"))]
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

// Detached background command: `sh -c` in its own process group on Unix
// (process_group(0) makes the stored pgid equal to the pid). Windows runs
// through `cmd /C` with a hidden console and no process group; its tree is
// stopped by taskkill /T.
#[cfg(unix)]
pub fn background_command(command: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).process_group(0);
    cmd
}

#[cfg(windows)]
pub fn background_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(command);
    super::hide_console(&mut cmd);
    cmd
}

// kill -0: succeeds while the target exists; a negative target addresses
// the whole process group. Windows has no process groups: the pid is probed
// through tasklist.
#[cfg(unix)]
pub fn kill_alive(target: &str) -> bool {
    Command::new("kill")
        .args(["-0", target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn kill_alive(target: &str) -> bool {
    let pid = target.trim_start_matches('-');
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut cmd = Command::new("tasklist");
    cmd.arg("/FI")
        .arg(format!("PID eq {pid}"))
        .args(["/NH", "/FO", "CSV"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    super::hide_console(&mut cmd);
    match cmd.output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")),
        Err(_) => false,
    }
}

// Which target the liveness probe of a tracked child addresses: the whole
// group on Unix, the pid itself on Windows.
#[cfg(unix)]
pub fn liveness_target(_pid: i64, pgid: i64) -> String {
    format!("-{pgid}")
}

#[cfg(windows)]
pub fn liveness_target(pid: i64, _pgid: i64) -> String {
    pid.to_string()
}

// Signal a tracked process: the POSIX signal goes to the whole group, so a
// dev server and its children die together. No POSIX signals on Windows:
// /T stops the tree, /F is the hard kill that replaces SIGKILL after the
// grace period.
#[cfg(unix)]
pub fn signal_group(_pid: i64, pgid: i64, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
pub fn signal_group(pid: i64, _pgid: i64, signal: &str) {
    let mut cmd = Command::new("taskkill");
    cmd.arg("/PID").arg(pid.to_string()).arg("/T");
    if signal == "-KILL" {
        cmd.arg("/F");
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    super::hide_console(&mut cmd);
    let _ = cmd.status();
}

// Foreground tool command (the `bash` agent tool): `sh -c` in its own process
// group on Unix, so the tool can signal the whole tree on cancel/timeout.
// Windows keeps `sh -c` as well, but without a process group: there the tree
// is stopped by pid in `kill_tool_child` below.
#[cfg(unix)]
pub fn tool_command(command: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).process_group(0);
    cmd
}

// Opt-in isolation for the `bash` tool (`sandbox: true`): the same `sh -c`,
// wrapped in a seatbelt profile that allows everything except sockets. The
// default stays unsandboxed on purpose — the tools must see the same network
// stack as Terminal.app, otherwise LAN access and SSH break (see netdiag).
// Returns `None` when the platform has no seatbelt (Windows, Linux).
#[cfg(target_os = "macos")]
pub fn sandboxed_tool_command(command: &str) -> Option<Command> {
    use std::os::unix::process::CommandExt;
    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
    const PROFILE: &str = "(version 1)(allow default)(deny network*)";
    if !Path::new(SANDBOX_EXEC).is_file() {
        return None;
    }
    let mut cmd = Command::new(SANDBOX_EXEC);
    cmd.arg("-p")
        .arg(PROFILE)
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .process_group(0);
    Some(cmd)
}

#[cfg(not(target_os = "macos"))]
pub fn sandboxed_tool_command(_command: &str) -> Option<Command> {
    None
}

// Windows has no guaranteed POSIX shell. Prefer Git Bash when it is installed:
// the agent and the built-in skills use bash syntax, and Git for Windows also
// brings the familiar coreutils. Without it, fall back to Windows PowerShell
// with UTF-8 output so the tool still works out of the box.
#[cfg(windows)]
pub fn bash_program() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("PROJECT_S_BASH").map(PathBuf::from) {
                if path.is_file() {
                    return Some(path);
                }
            }
            let known = [
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Program Files\Git\usr\bin\bash.exe",
                r"C:\Program Files (x86)\Git\bin\bash.exe",
                r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
            ];
            if let Some(path) = known
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
            {
                return Some(path);
            }
            which("bash.exe")
        })
        .clone()
}

// Resolve an executable through PATH without pulling in a shell.
#[cfg(windows)]
pub fn which(name: &str) -> Option<PathBuf> {
    let mut cmd = Command::new("where.exe");
    cmd.arg(name).stdout(Stdio::piped()).stderr(Stdio::null());
    super::hide_console(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
pub fn configure_bash_env(cmd: &mut Command, bash: &Path) {
    let Some(root) = bash.parent().and_then(Path::parent) else {
        return;
    };
    let mut paths = vec![
        root.join("usr/bin"),
        root.join("mingw64/bin"),
        root.join("bin"),
    ];
    if let Some(old) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&old));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        cmd.env("PATH", joined);
    }
}

#[cfg(windows)]
pub fn tool_command(command: &str) -> Command {
    if let Some(bash) = bash_program() {
        let mut cmd = Command::new(&bash);
        configure_bash_env(&mut cmd, &bash);
        cmd.arg("-c").arg(command);
        return cmd;
    }

    use base64::Engine as _;
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8; {command}"
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
    ])
    .arg(encoded);
    super::hide_console(&mut cmd);
    cmd
}

#[cfg(not(any(unix, windows)))]
pub fn tool_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

// A Windows Job Object gives the foreground tool a hard ownership boundary:
// every descendant is in the job, and KILL_ON_JOB_CLOSE means even a child
// that detaches or ignores taskkill cannot outlive the guard. On other
// platforms this is a no-op marker; Unix already uses a process group.
#[cfg(windows)]
pub struct ToolJob(HANDLE);

#[cfg(not(windows))]
pub struct ToolJob;

#[cfg(windows)]
impl ToolJob {
    pub fn attach(child: &Child) -> Option<ToolJob> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }
            let process = child.as_raw_handle() as HANDLE;
            if AssignProcessToJobObject(job, process) == 0 {
                CloseHandle(job);
                return None;
            }
            Some(ToolJob(job))
        }
    }

    fn kill(&self) {
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ToolJob {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
pub fn attach_tool_job(_child: &Child) -> Option<ToolJob> {
    None
}

#[cfg(windows)]
pub fn attach_tool_job(child: &Child) -> Option<ToolJob> {
    ToolJob::attach(child)
}

// Hard kill of a foreground tool child: the whole group on Unix, where
// `tool_command` made the pgid equal to the pid, so a dev server and its
// children die together and the stdout/stderr pipes the tool's reader threads
// are blocked on finally close.
#[cfg(unix)]
pub fn kill_tool_child(child: &mut Child, _job: Option<&ToolJob>) {
    let pid = child.id() as i64;
    signal_group(pid, pid, "-KILL");
}

// Windows has no POSIX process groups. `/T` walks the child tree by parent
// pid, so the `sh` wrapper, the command it started, and that command's own
// children (a dev server) are stopped together; `/F` is the hard kill that
// matches SIGKILL. Killing only the direct child would orphan the grandchildren
// and leave the stdout/stderr pipes the tool's reader threads block on open.
#[cfg(windows)]
pub fn kill_tool_child(child: &mut Child, job: Option<&ToolJob>) {
    if let Some(job) = job {
        job.kill();
        let _ = child.kill();
        return;
    }
    let mut cmd = Command::new("taskkill");
    cmd.arg("/PID")
        .arg(child.id().to_string())
        .arg("/T")
        .arg("/F")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::hide_console(&mut cmd);
    let _ = cmd.status();
    // taskkill can fail (already-exited child, missing binary, permissions);
    // the direct kill is the safety net either way.
    let _ = child.kill();
}

// No process-group or tree kill on other targets: direct child only.
#[cfg(not(any(unix, windows)))]
pub fn kill_tool_child(child: &mut Child, _job: Option<&ToolJob>) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    // The stop path signals the group, not just the leader: a group target
    // is what keeps a dev server's children from surviving `process_stop`.
    #[cfg(unix)]
    #[test]
    fn liveness_target_addresses_the_process_group() {
        assert_eq!(liveness_target(4242, 4242), "-4242");
    }

    #[test]
    fn kill_alive_is_false_for_a_pid_that_never_existed() {
        assert!(!kill_alive("4294967295"));
    }
}
