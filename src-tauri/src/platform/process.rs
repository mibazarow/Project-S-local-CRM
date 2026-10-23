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

#[cfg(not(unix))]
pub fn tool_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

// Hard kill of a foreground tool child: the whole group on Unix, where
// `tool_command` made the pgid equal to the pid, so a dev server and its
// children die together and the stdout/stderr pipes the tool's reader threads
// are blocked on finally close.
#[cfg(unix)]
pub fn kill_tool_child(child: &mut Child) {
    let pid = child.id() as i64;
    signal_group(pid, pid, "-KILL");
}

// Windows has no POSIX process groups. `/T` walks the child tree by parent
// pid, so the `sh` wrapper, the command it started, and that command's own
// children (a dev server) are stopped together; `/F` is the hard kill that
// matches SIGKILL. Killing only the direct child would orphan the grandchildren
// and leave the stdout/stderr pipes the tool's reader threads block on open.
#[cfg(windows)]
pub fn kill_tool_child(child: &mut Child) {
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
pub fn kill_tool_child(child: &mut Child) {
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
