// skill_exec (SPEC 6.2 script skills): run the declared entrypoint of a
// script skill. The confirmation queue is the guard (Mutating/Check), like
// bash; the hard limits are structural: cwd pinned to the skill folder, the
// entrypoint resolved strictly inside it, TMPDIR/TEMP/TMP pointed at
// Harness/tmp/, a 120 s kill-timeout (a manifest may ask for less), a 30k
// per-stream output cap, and args passed as argv — never through a shell.

use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use crate::paths;
use super::manifest::{entrypoint, find, SkillKind};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Hard safety cap for skill_exec: a manifest may ask for less, never more.
pub const SCRIPT_EXEC_TIMEOUT_SEC: u64 = 120;
// Per-stream stdout/stderr cap for skill_exec (same size as the bash tool).
const EXEC_OUTPUT_CAP: usize = 30_000;

fn is_executable(path: &Path) -> bool {
    crate::platform::is_executable(path)
}

// Joins the declared entrypoint onto the skill folder and keeps it strictly
// inside: absolute paths, traversal components and symlink escapes (the
// canonicalized target leaves the canonicalized folder) are refused.
fn resolve_entry(skill_dir: &Path, entry: &str) -> std::result::Result<PathBuf, String> {
    let rel = Path::new(entry);
    if rel.is_absolute()
        || rel.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err(format!("entrypoint \"{entry}\" escapes the skill folder"));
    }
    let candidate = skill_dir.join(rel);
    if let (Ok(dir), Ok(file)) = (skill_dir.canonicalize(), candidate.canonicalize()) {
        if !file.starts_with(&dir) {
            return Err(format!("entrypoint \"{entry}\" escapes the skill folder"));
        }
        return Ok(file);
    }
    // Missing files fall through to the not_found check in the tool.
    Ok(candidate)
}

// Runner selection: the executable bit wins (the file's own shebang runs
// it); otherwise the extension picks the interpreter. Anything else is
// refused. Returns the command plus the program label for error messages.
fn command_for(candidate: &Path) -> std::result::Result<(Command, String), String> {
    if is_executable(candidate) {
        let label = candidate.display().to_string();
        return Ok((Command::new(candidate), label));
    }
    let ext = candidate
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    #[cfg(windows)]
    let (interpreter, extra_args): (String, &[&str]) = match ext.as_str() {
        // The py launcher is present on most Windows installs; python.exe is
        // preferred when it is on PATH, otherwise `py -3` finds the default
        // Python 3 installation without hard-coding a version.
        "py" => {
            if crate::platform::process::which("python.exe").is_some() {
                ("python".into(), &[])
            } else if crate::platform::process::which("py.exe").is_some() {
                ("py".into(), &["-3"])
            } else {
                return Err(
                    "Python not found: install Python 3 (python.exe or the py launcher) to run .py skills"
                        .into(),
                );
            }
        }
        "sh" | "bash" => match crate::platform::process::bash_program() {
            Some(path) => (path.to_string_lossy().into_owned(), &["-l"]),
            None => {
                return Err(
                    "bash not found: install Git for Windows (Git Bash) to run .sh/.bash skills, or use a .py/.js skill"
                        .into(),
                )
            }
        },
        "js" | "mjs" => ("node".into(), &[]),
        _ => {
            return Err(format!(
                "no runner for entrypoint \"{}\": set the executable bit or use a .py/.sh/.bash/.js/.mjs file",
                candidate.display()
            ))
        }
    };
    #[cfg(not(windows))]
    let (interpreter, extra_args): (String, &[&str]) = match ext.as_str() {
        "py" => ("python3".into(), &[]),
        "sh" | "bash" => ("bash".into(), &[]),
        "js" | "mjs" => ("node".into(), &[]),
        _ => {
            return Err(format!(
                "no runner for entrypoint \"{}\": set the executable bit or use a .py/.sh/.bash/.js/.mjs file",
                candidate.display()
            ))
        }
    };
    let mut cmd = Command::new(&interpreter);
    // Windows canonicalize() returns a \\?\ verbatim path. Git Bash (and
    // MSYS path conversion) mangles that prefix into a non-existent path, so
    // hand the interpreter the ordinary drive path instead.
    cmd.args(extra_args)
        .arg(crate::platform::path_string(candidate));
    Ok((cmd, interpreter))
}

// Reads a pipe up to the cap plus one byte so truncation is detectable even
// if the script keeps producing (same pattern as the bash tool).
fn read_capped(mut reader: impl Read, cap: usize) -> (String, bool) {
    let mut buf = Vec::new();
    let _ = (&mut reader).take((cap as u64) + 1).read_to_end(&mut buf);
    let truncated = buf.len() > cap;
    buf.truncate(cap);
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

pub struct SkillExecTool;

impl Tool for SkillExecTool {
    fn name(&self) -> &str {
        "skill_exec"
    }
    fn description(&self) -> &str {
        if cfg!(windows) {
            "Run a script skill's declared entrypoint (entry: or scripts: in its SKILL.md) with the given args, passed as argv — never through a shell. Runs with cwd = the skill folder, TMPDIR/TEMP/TMP pointed at Harness/tmp/, killed after 120 s (a script skill may declare a shorter timeout_sec), stdout/stderr capped at 30k each. The executable bit runs the file directly; otherwise .py runs via python or py -3, .sh/.bash via Git Bash when installed, .js/.mjs via node. Returns exitCode, stdout, stderr, truncated. Dangerous: always confirmed."
        } else {
            "Run a script skill's declared entrypoint (entry: or scripts: in its SKILL.md) with the given args, passed as argv — never through a shell. Runs with cwd = the skill folder, TMPDIR/TEMP/TMP pointed at Harness/tmp/, killed after 120 s (a script skill may declare a shorter timeout_sec), stdout/stderr capped at 30k each. The executable bit runs the file directly; otherwise .py runs via python3, .sh/.bash via bash, .js/.mjs via node. Returns exitCode, stdout, stderr, truncated. Dangerous: always confirmed."
        }
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Check
    }
    fn danger_reason(&self, args: &Value, _ctx: &ToolCtx) -> Option<String> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("?");
        Some(format!("запуск скрипта скилла «{name}»"))
    }
    fn max_result_len(&self) -> usize {
        80_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = ctx.working_dir.clone().ok_or(AppError::NotConfigured)?;
        let name = args["name"].as_str().unwrap_or_default();
        let skill = match find(&wd, name) {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                let code = if msg.contains("skill disabled") {
                    ToolErrorCode::Hidden
                } else {
                    ToolErrorCode::NotFound
                };
                return Ok(ToolResult::err(code, msg));
            }
        };
        let Some(entry) = entrypoint(&skill) else {
            return Ok(ToolResult::err(
                ToolErrorCode::InvalidArgs,
                format!(
                    "skill \"{}\" has no entrypoint: declare \"entry:\" or a \"scripts:\" list in its SKILL.md frontmatter",
                    skill.name
                ),
            ));
        };
        let Some(skill_dir) = skill.path.parent() else {
            return Ok(ToolResult::err(
                ToolErrorCode::ExecutionError,
                format!("skill path {} has no parent folder", skill.path.display()),
            ));
        };
        let candidate = match resolve_entry(skill_dir, &entry) {
            Ok(p) => p,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::PermissionDenied, msg)),
        };
        if !candidate.is_file() {
            return Ok(ToolResult::err(
                ToolErrorCode::NotFound,
                format!("entrypoint not found: {}", candidate.display()),
            ));
        }
        let (mut cmd, program) = match command_for(&candidate) {
            Ok(pair) => pair,
            Err(msg) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, msg)),
        };
        // Args go straight into argv; validation already guarantees strings,
        // so no shell ever sees them and no interpolation is possible.
        if let Some(items) = args.get("args").and_then(Value::as_array) {
            for item in items {
                cmd.arg(item.as_str().unwrap_or_default());
            }
        }
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(&tmp)?;
        let timeout_secs = match skill.kind {
            SkillKind::Script { timeout_sec } => timeout_sec.clamp(1, SCRIPT_EXEC_TIMEOUT_SEC),
            _ => SCRIPT_EXEC_TIMEOUT_SEC,
        };
        let workdir = std::path::PathBuf::from(crate::platform::path_string(skill_dir));
        let mut child = match cmd
            .current_dir(workdir)
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("interpreter \"{program}\" not found on PATH"),
                ));
            }
            Err(e) => return Err(AppError::Io(format!("skill script spawn: {e}"))),
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_thread = std::thread::spawn(move || stdout.map(|r| read_capped(r, EXEC_OUTPUT_CAP)));
        let err_thread = std::thread::spawn(move || stderr.map(|r| read_capped(r, EXEC_OUTPUT_CAP)));
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ToolResult::err(
                    ToolErrorCode::ExecutionError,
                    format!("skill script timed out after {timeout_secs} s and was killed"),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let (stdout, out_cut) = out_thread.join().unwrap_or_default().unwrap_or_default();
        let (stderr, err_cut) = err_thread.join().unwrap_or_default().unwrap_or_default();
        Ok(
            ToolResult::ok(json!({
                "skill": skill.name,
                "entrypoint": entry,
                "exitCode": status.code().unwrap_or(-1),
                "stdout": stdout,
                "stderr": stderr,
                "truncated": out_cut || err_cut,
            }))
            .capped(self.max_result_len()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use crate::skills::testutil::write_skill;

    // A user skill whose frontmatter is `frontmatter` plus a script `file`;
    // returns the skill folder.
    fn write_exec_skill(
        wd: &Path,
        folder: &str,
        frontmatter: &str,
        file: &str,
        content: &str,
    ) -> PathBuf {
        let dir = paths::skills_user_dir(wd).join(folder);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {folder}\ndescription: exec test\n{frontmatter}---\nRun.\n"),
        )
        .unwrap();
        fs::write(dir.join(file), content).unwrap();
        dir
    }

    fn have_interpreter(prog: &str) -> bool {
        Command::new(prog)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn exec_ctx(wd: &Path) -> ToolCtx {
        ToolCtx {
            working_dir: Some(wd.to_path_buf()),
            ..Default::default()
        }
    }

    #[test]
    fn skill_exec_runs_sh_script_with_argv_tmpdir_and_cwd() {
        let wd = temp_test_dir("skills-exec-sh");
        write_exec_skill(
            &wd,
            "echoer",
            "type: script\nentry: run.sh\n",
            "run.sh",
            "echo \"cwd:$(basename \"$PWD\")\"; echo \"tmp:$TMPDIR\"; echo \"a1=$1\"; echo \"a2=$2\"\n",
        );
        let r = SkillExecTool
            .execute(&json!({ "name": "echoer", "args": ["hello", "; rm -rf /"] }), &exec_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["skill"], "echoer");
        assert_eq!(data["entrypoint"], "run.sh");
        assert_eq!(data["exitCode"], 0, "{data}");
        let stdout = data["stdout"].as_str().unwrap();
        assert!(stdout.contains("cwd:echoer"), "cwd must be the skill folder: {stdout}");
        assert!(
            stdout.contains(&format!("tmp:{}", paths::harness_tmp_dir(&wd).display())),
            "TMPDIR must point at Harness/tmp: {stdout}"
        );
        assert!(stdout.contains("a1=hello"), "{stdout}");
        assert!(stdout.contains("a2=; rm -rf /"), "argv must stay literal: {stdout}");
        assert_eq!(data["truncated"], false);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_runs_an_entrypoint_declared_by_a_non_script_skill() {
        let wd = temp_test_dir("skills-exec-any-kind");
        write_exec_skill(&wd, "tooly", "entry: go.sh\n", "go.sh", "echo tooly-ran\n");
        let r = SkillExecTool
            .execute(&json!({ "name": "tooly" }), &exec_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        assert!(data["stdout"].as_str().unwrap().contains("tooly-ran"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn skill_exec_prefers_the_executable_bit_over_the_extension() {
        use std::os::unix::fs::PermissionsExt;
        let wd = temp_test_dir("skills-exec-bit");
        let dir = write_exec_skill(
            &wd,
            "bitter",
            "type: script\nentry: run.py\n",
            "run.py",
            "#!/bin/sh\necho direct\n",
        );
        fs::set_permissions(dir.join("run.py"), fs::Permissions::from_mode(0o755)).unwrap();
        let r = SkillExecTool
            .execute(&json!({ "name": "bitter" }), &exec_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        // python3 would choke on the sh shebang; the direct run succeeds.
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        assert_eq!(data["stdout"], "direct\n");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_runs_python_script_when_the_interpreter_exists() {
        if !have_interpreter("python3") {
            return; // graceful skip: no python3 on this machine
        }
        let wd = temp_test_dir("skills-exec-py");
        write_exec_skill(
            &wd,
            "pyone",
            "type: script\nentry: go.py\n",
            "go.py",
            "import sys\nprint('py', sys.argv[1])\n",
        );
        let r = SkillExecTool
            .execute(&json!({ "name": "pyone", "args": ["42"] }), &exec_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["exitCode"], 0, "{data}");
        assert_eq!(data["stdout"], "py 42\n");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_caps_output_at_30k_per_stream() {
        let wd = temp_test_dir("skills-exec-cap");
        write_exec_skill(
            &wd,
            "chatty",
            "type: script\nentry: run.sh\n",
            "run.sh",
            "yes x | head -c 40000\n",
        );
        let r = SkillExecTool
            .execute(&json!({ "name": "chatty" }), &exec_ctx(&wd))
            .unwrap();
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["stdout"].as_str().unwrap().chars().count(), 30_000, "{data}");
        assert_eq!(data["truncated"], true);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_requires_a_declared_entrypoint() {
        let wd = temp_test_dir("skills-exec-noentry");
        write_exec_skill(&wd, "bare", "type: script\n", "run.sh", "echo x\n");
        write_skill(
            &paths::skills_user_dir(&wd),
            "plain",
            "---\nname: plain\ndescription: d\n---\nB.\n",
        );
        let ctx = exec_ctx(&wd);
        for name in ["bare", "plain"] {
            let r = SkillExecTool.execute(&json!({ "name": name }), &ctx).unwrap();
            let err = r.error.unwrap();
            assert_eq!(err.code, "invalid_args", "{name}");
            assert!(err.message.contains("entrypoint"), "{}", err.message);
        }
        let r = SkillExecTool.execute(&json!({ "name": "ghost" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "not_found");
        fs::write(paths::skills_enabled_file(&wd), "{ \"disabled\": [\"bare\"] }").unwrap();
        let r = SkillExecTool.execute(&json!({ "name": "bare" }), &ctx).unwrap();
        assert_eq!(r.error.unwrap().code, "hidden");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_rejects_entrypoints_that_escape_the_skill_folder() {
        let wd = temp_test_dir("skills-exec-escape");
        fs::create_dir_all(paths::skills_user_dir(&wd)).unwrap();
        fs::write(paths::skills_user_dir(&wd).join("evil.sh"), "echo pwned\n").unwrap();
        write_exec_skill(&wd, "upward", "type: script\nentry: ../evil.sh\n", "run.sh", "echo x\n");
        write_exec_skill(&wd, "abs", "type: script\nentry: /etc/hosts\n", "run.sh", "echo x\n");
        let ctx = exec_ctx(&wd);
        for name in ["upward", "abs"] {
            let r = SkillExecTool.execute(&json!({ "name": name }), &ctx).unwrap();
            let err = r.error.unwrap();
            assert_eq!(err.code, "permission_denied", "{name}");
            assert!(err.message.contains("escapes"), "{}", err.message);
        }

        // a symlink entry resolving outside the skill folder is refused too
        #[cfg(unix)]
        {
            let dir = write_exec_skill(
                &wd,
                "linked",
                "type: script\nentry: link.sh\n",
                "run.sh",
                "echo x\n",
            );
            fs::write(wd.join("evil2.sh"), "echo pwned\n").unwrap();
            std::os::unix::fs::symlink(wd.join("evil2.sh"), dir.join("link.sh")).unwrap();
            let r = SkillExecTool.execute(&json!({ "name": "linked" }), &ctx).unwrap();
            let err = r.error.unwrap();
            assert_eq!(err.code, "permission_denied");
            assert!(err.message.contains("escapes"), "{}", err.message);
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_missing_entrypoint_file_is_not_found() {
        let wd = temp_test_dir("skills-exec-missing");
        write_exec_skill(&wd, "gone", "type: script\nentry: nothere.sh\n", "run.sh", "echo x\n");
        let r = SkillExecTool
            .execute(&json!({ "name": "gone" }), &exec_ctx(&wd))
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("entrypoint not found"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_rejects_entrypoints_without_a_runner() {
        let wd = temp_test_dir("skills-exec-runner");
        write_exec_skill(&wd, "opaque", "type: script\nentry: tool.dat\n", "tool.dat", "data\n");
        let r = SkillExecTool
            .execute(&json!({ "name": "opaque" }), &exec_ctx(&wd))
            .unwrap();
        let err = r.error.unwrap();
        assert_eq!(err.code, "invalid_args");
        assert!(err.message.contains("no runner"), "{}", err.message);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_times_out_and_kills_the_script() {
        let wd = temp_test_dir("skills-exec-timeout");
        write_exec_skill(
            &wd,
            "sleeper",
            "type: script\ntimeout_sec: 1\nentry: run.sh\n",
            "run.sh",
            "sleep 30\n",
        );
        let started = Instant::now();
        let r = SkillExecTool
            .execute(&json!({ "name": "sleeper" }), &exec_ctx(&wd))
            .unwrap();
        let elapsed = started.elapsed();
        let err = r.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("timed out"), "{}", err.message);
        assert!(elapsed < Duration::from_secs(10), "kill took {elapsed:?}");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn skill_exec_is_mutating_check_with_named_danger_reason_and_strict_schema() {
        use crate::agent::tool::validate_args;
        assert_eq!(SkillExecTool.class(), ToolClass::Mutating);
        assert_eq!(SkillExecTool.policy(), PermissionPolicy::Check);
        assert_eq!(
            SkillExecTool
                .danger_reason(&json!({ "name": "demo" }), &ToolCtx::default())
                .as_deref(),
            Some("запуск скрипта скилла «demo»")
        );
        let schema = SkillExecTool.parameters();
        assert!(validate_args(&schema, &json!({ "name": "x" })).is_ok());
        assert!(validate_args(&schema, &json!({ "name": "x", "args": ["a", "b"] })).is_ok());
        assert!(validate_args(&schema, &json!({})).unwrap_err().contains("name"));
        assert!(validate_args(&schema, &json!({ "name": "x", "bogus": 1 }))
            .unwrap_err()
            .contains("bogus"));
        assert!(validate_args(&schema, &json!({ "name": "x", "args": [1] }))
            .unwrap_err()
            .contains("string"));
    }
}
