// The bash tool (SPEC 5.2 item 2): sh -c with the default cwd Harness/,
// TMPDIR/TEMP/TMP pointed at Harness/tmp/ (never the system /tmp), a wall
// timeout that kills the command, and capped stdout/stderr. The background
// twin lives in process_tools.rs.
use crate::agent::project_tools::require_wd;
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use super::resolve_path;
use crate::error::Result;
use crate::paths;
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
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
    pub fn with_timeout(timeout: Duration) -> Self {
        BashTool { timeout }
    }
}

// Reads a pipe up to the cap plus one byte so truncation is detectable even
// if the command keeps producing (then the pipe fills and the timeout kills
// the command — the output is capped anyway).
fn read_capped(mut reader: impl Read) -> (String, bool) {
    let mut buf = Vec::new();
    let _ = (&mut reader)
        .take((BASH_OUTPUT_CAP as u64) + 1)
        .read_to_end(&mut buf);
    let truncated = buf.len() > BASH_OUTPUT_CAP;
    buf.truncate(BASH_OUTPUT_CAP);
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command via sh -c (default cwd: Harness/). TMPDIR/TEMP/TMP point at Harness/tmp/, never the system /tmp. Returns exitCode, stdout, stderr; 120 s timeout."
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
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || stdout.map(read_capped));
        let err_thread = std::thread::spawn(move || stderr.map(read_capped));
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
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
}
