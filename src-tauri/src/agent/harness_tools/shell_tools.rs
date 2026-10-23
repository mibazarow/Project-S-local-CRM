// The bash tool (SPEC 5.2 item 2): sh -c with the default cwd Harness/,
// TMPDIR/TEMP/TMP pointed at Harness/tmp/ (never the system /tmp), a wall
// timeout that kills the command and everything it spawned (see the platform
// process seam), and capped stdout/stderr. The background twin lives in
// process_tools.rs.
use crate::agent::project_tools::require_wd;
use crate::agent::result::{read_capped, ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::resolve_path;
use crate::error::Result;
use crate::paths;
use crate::platform;
use serde_json::{json, Value};
use std::fs;
use std::process::Stdio;
use std::time::{Duration, Instant};

const BASH_TIMEOUT_SECS: u64 = 120;
const BASH_OUTPUT_CAP: usize = 30_000;

pub struct BashTool {
    timeout: Duration,
}

impl Default for BashTool {
    fn default() -> Self {
        BashTool {
            timeout: Duration::from_secs(BASH_TIMEOUT_SECS),
        }
    }
}

impl BashTool {
    #[cfg(test)]
    pub fn with_timeout(timeout: Duration) -> Self {
        BashTool { timeout }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        if cfg!(windows) {
            "Run a shell command. On Windows it uses Git Bash when installed, otherwise Windows PowerShell with UTF-8 output (default cwd: Harness/). TMPDIR/TEMP/TMP point at Harness/tmp/, never the system temp. Returns exitCode, stdout, stderr; 120 s timeout."
        } else {
            "Run a shell command via sh -c (default cwd: Harness/). TMPDIR/TEMP/TMP point at Harness/tmp/, never the system /tmp. Returns exitCode, stdout, stderr; 120 s timeout."
        }
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "cwd": { "type": "string" }
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
    fn danger_reason(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        Some("выполнение произвольной shell-команды".to_string())
    }
    fn max_result_len(&self) -> usize {
        80_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let command = args["command"].as_str().unwrap_or_default();
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp)?;
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(c) if !c.is_empty() => resolve_path(c, &wd),
            _ => paths::harness_dir(&wd),
        };
        fs::create_dir_all(&cwd)?;
        let mut child = platform::process::tool_command(command)
            .current_dir(&cwd)
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let job = platform::process::attach_tool_job(&child);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, BASH_OUTPUT_CAP)));
        let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, BASH_OUTPUT_CAP)));
        let deadline = Instant::now() + self.timeout;
        let cancel = ctx.cancel.clone();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if cancel.as_ref().is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
                platform::process::kill_tool_child(&mut child, job.as_ref());
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    "command was cancelled by user",
                ));
            }
            if Instant::now() >= deadline {
                platform::process::kill_tool_child(&mut child, job.as_ref());
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("command timed out after {} s and was killed", self.timeout.as_secs_f64()),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let (stdout, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
        let (stderr, err_cut) = err_thread.join().unwrap_or_default().unwrap_or_default();
        Ok(ToolResult::ok(json!({
            "cwd": cwd.display().to_string(),
            "exitCode": status.code().unwrap_or(-1),
            "stdout": stdout,
            "stderr": stderr,
            "truncated": out_cut || err_cut,
        }))
        .capped(self.max_result_len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness_tools::test_support::{exec, setup};
    #[cfg(unix)]
    use crate::onboarding::temp_test_dir;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    #[test]
    fn bash_runs_with_tmp_env_default_cwd_and_exit_codes() {
        let env = setup("ht-bash");
        let r = exec("bash", json!({ "command": "echo hi" }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0);
        assert_eq!(data["stdout"], "hi\n");
        assert_eq!(data["cwd"], paths::harness_dir(&env.wd).display().to_string());

        let r = exec("bash", json!({ "command": "exit 3" }), &env.ctx);
        assert!(r.ok);
        assert_eq!(r.data.unwrap()["exitCode"], 3);

        let r = exec("bash", json!({ "command": "echo \"$TMPDIR|$TEMP|$TMP\"" }), &env.ctx);
        let out = r.data.unwrap()["stdout"].as_str().unwrap().to_owned();
        let tmp = paths::harness_tmp_dir(&env.wd);
        for part in out.trim().split('|') {
            assert!(!part.is_empty(), "empty tmp var in {out:?}");
            assert!(Path::new(part).ends_with("Harness/tmp"), "{part} not under Harness/tmp");
            assert_eq!(PathBuf::from(part), tmp);
        }

        let r = exec("bash", json!({ "command": "pwd", "cwd": "sub" }), &env.ctx);
        // pwd resolves symlinks (/var -> /private/var on macOS), so compare
        // against the canonicalized cwd.
        assert_eq!(
            r.data.unwrap()["stdout"].as_str().unwrap().trim(),
            env.wd.join("sub").canonicalize().unwrap().display().to_string()
        );
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn bash_timeout_kills_the_command() {
        let env = setup("ht-bash-timeout");
        let tool = BashTool::with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let r = tool.execute(&json!({ "command": "sleep 5" }), &env.ctx).unwrap();
        let elapsed = started.elapsed();
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("timed out"), "{}", err.message);
        assert!(elapsed < Duration::from_secs(4), "kill took {elapsed:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn bash_cancel_kills_the_child_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let env = setup("ht-bash-cancel");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let mut ctx = env.ctx.clone();
        ctx.cancel = Some(std::sync::Arc::clone(&cancel));
        let flag = std::sync::Arc::clone(&cancel);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let r = BashTool::default()
            .execute(&json!({ "command": "sleep 30" }), &ctx)
            .unwrap();
        let elapsed = started.elapsed();
        setter.join().unwrap();
        assert!(!r.ok);
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("cancelled"), "{}", err.message);
        // The child must die on cancel, not after the 120 s command timeout.
        assert!(elapsed < Duration::from_secs(5), "cancel took {elapsed:?}");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    // Unix group kill (R-24): the tool signals the whole process group, so a
    // grandchild dies with the `sh` wrapper and stops holding the stdout/stderr
    // pipes the two reader threads are blocked on. The probe command leaves the
    // grandchild pid in a file and then sleeps; `sleep 30 &` stays in the
    // wrapper's group because a non-interactive shell has no job control.
    #[cfg(unix)]
    fn group_probe(pidfile: &Path) -> String {
        format!("sleep 30 & echo $! > '{}'; sleep 30", pidfile.display())
    }

    #[cfg(unix)]
    fn grandchild_pid(pidfile: &Path) -> u32 {
        for _ in 0..100 {
            if let Ok(raw) = fs::read_to_string(pidfile) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    if pid > 1 {
                        return pid;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no grandchild pid in {}", pidfile.display());
    }

    #[cfg(unix)]
    fn pid_is_gone(pid: u32) -> bool {
        for _ in 0..100 {
            if !platform::process::kill_alive(&pid.to_string()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    // Characterization of what this slice fixes: the old direct-child kill
    // leaves the grandchild alive.
    #[cfg(unix)]
    #[test]
    fn killing_only_the_direct_child_leaves_the_grandchild_alive() {
        use std::process::Command;
        let dir = temp_test_dir("ht-bash-group-old");
        let pidfile = dir.join("grandchild.pid");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(group_probe(&pidfile))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = grandchild_pid(&pidfile);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            platform::process::kill_alive(&pid.to_string()),
            "grandchild {pid} should have survived a direct-child kill"
        );
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        assert!(pid_is_gone(pid), "cleanup of grandchild {pid} failed");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bash_timeout_kills_the_whole_process_group() {
        let env = setup("ht-bash-group-timeout");
        let pidfile = env.wd.join("grandchild.pid");
        let tool = BashTool::with_timeout(Duration::from_millis(300));
        let r = tool
            .execute(&json!({ "command": group_probe(&pidfile) }), &env.ctx)
            .unwrap();
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("timed out"));
        let pid = grandchild_pid(&pidfile);
        assert!(pid_is_gone(pid), "grandchild {pid} survived the timeout kill");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bash_cancel_kills_the_whole_process_group() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let env = setup("ht-bash-group-cancel");
        let pidfile = env.wd.join("grandchild.pid");
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let mut ctx = env.ctx.clone();
        ctx.cancel = Some(std::sync::Arc::clone(&cancel));
        let flag = std::sync::Arc::clone(&cancel);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
        });
        let r = BashTool::default()
            .execute(&json!({ "command": group_probe(&pidfile) }), &ctx)
            .unwrap();
        setter.join().unwrap();
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("cancelled"));
        let pid = grandchild_pid(&pidfile);
        assert!(pid_is_gone(pid), "grandchild {pid} survived the cancel kill");
        fs::remove_dir_all(&env.wd).unwrap();
    }
}
