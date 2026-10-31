// Background process control tools (harness): the background twin of bash
// (SPEC 5.2 #2) for long-running commands — dev servers, installs. bash
// stays foreground-only with its 120 s cap; process_start detaches a
// command into its own process group with output appended to a log under
// Harness/tmp/processes/, process_logs tails that log, process_stop
// signals the group (TERM, then KILL after ~2 s) and process_list shows
// and reconciles the rows. All process mechanics live in the processes
// domain module; these tools only validate args and shape envelopes.
// Catalog placement: all four are lazy-discovered (deferred) together
// with bash — the agent that needs background processes enables them the
// same way it enables bash; making list/logs/stop core would put four
// rarely-used schemas into every first round for a capability that only
// matters once the machine-wide set is already in play.
use super::harness_tools::resolve_path;
use super::project_tools::{envelope, require_wd, with_conn};
use super::result::{ToolErrorCode, ToolResult};
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::processes;
use serde_json::{json, Value};
use std::path::Path;

pub struct ProcessStartTool;

impl Tool for ProcessStartTool {
    fn name(&self) -> &str {
        "process_start"
    }
    fn description(&self) -> &str {
        "Start a long-running shell command in the background via sh -c (dev servers, installs): it keeps running after the call returns, stdout+stderr are appended to a log under Harness/tmp/processes/, and it runs in its own process group; TMPDIR/TEMP/TMP point at Harness/tmp. Returns id, pid, logPath; watch with process_logs, stop with process_stop."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "cwd": { "type": "string" },
                "name": { "type": "string" }
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
        Some("запуск фоновой shell-команды".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let command = args["command"].as_str().unwrap_or_default();
        if command.trim().is_empty() {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "command is empty"));
        }
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(|c| resolve_path(c, &wd));
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let r = with_conn(ctx, |conn| processes::start(conn, &wd, command, cwd.as_deref(), name));
        Ok(match r {
            Ok(p) => ToolResult::ok(json!({ "id": p.id, "pid": p.pid, "logPath": p.log_path })),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

pub struct ProcessLogsTool;

impl Tool for ProcessLogsTool {
    fn name(&self) -> &str {
        "process_logs"
    }
    fn description(&self) -> &str {
        "Tail the log of a background process: the last lines (default 200) cut to the last maxChars (default 20000); returns id, status (reconciled) and log."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "number" },
                "lines": { "type": "number" },
                "maxChars": { "type": "number" }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }
    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }
    fn max_result_len(&self) -> usize {
        30_000
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let lines = args
            .get("lines")
            .and_then(Value::as_f64)
            .unwrap_or(processes::DEFAULT_LOG_LINES as f64)
            .clamp(1.0, 5000.0) as usize;
        let max_chars = args
            .get("maxChars")
            .and_then(Value::as_f64)
            .unwrap_or(processes::DEFAULT_LOG_CHARS as f64)
            .clamp(200.0, 100_000.0) as usize;
        let r = with_conn(ctx, |conn| processes::get_reconciled(conn, id));
        Ok(match r {
            Ok(p) => {
                let log = processes::tail_log(Path::new(&p.log_path), lines, max_chars)?;
                ToolResult::ok(json!({ "id": p.id, "status": p.status, "log": log }))
                    .capped(self.max_result_len())
            }
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

pub struct ProcessStopTool;

impl Tool for ProcessStopTool {
    fn name(&self) -> &str {
        "process_stop"
    }
    fn description(&self) -> &str {
        "Stop a background process by id: SIGTERM to its whole process group, then SIGKILL after ~2 s if still alive; marks the row stopped. Dangerous: always confirmed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "id": { "type": "number" } },
            "required": ["id"],
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
        Some("остановка фонового процесса".to_string())
    }
    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
        let r = with_conn(ctx, |conn| processes::stop(conn, id));
        Ok(envelope(r))
    }
}

pub struct ProcessListTool;

impl Tool for ProcessListTool {
    fn name(&self) -> &str {
        "process_list"
    }
    fn description(&self) -> &str {
        "List background processes (newest first) with status, pid, pgid, command, cwd, log path and exit code; dead pids are reconciled to exited/error first."
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
    fn max_result_len(&self) -> usize {
        30_000
    }
    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let r = with_conn(ctx, processes::list);
        Ok(match r {
            Ok(list) => ToolResult::ok(json!({ "processes": list })).capped(self.max_result_len()),
            Err(e) => envelope::<()>(Err(e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::catalog::Resolution;
    use crate::agent::harness_tools::harness_catalog;
    use crate::agent::tool::validate_args;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::state::AppState;
    use rusqlite::params;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct Env {
        wd: PathBuf,
        ctx: ToolCtx,
    }

    fn setup(tag: &str) -> Env {
        let wd = temp_test_dir(tag);
        std::fs::create_dir_all(crate::paths::harness_dir(&wd)).unwrap();
        let conn = db::open(&crate::paths::db_path(&wd)).unwrap();
        let state = std::sync::Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let ctx = ToolCtx {
            scope: None,
            cancel: None,
            project_id: None,
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        Env { wd, ctx }
    }

    fn tool(name: &str) -> std::sync::Arc<dyn Tool> {
        let catalog = harness_catalog();
        match catalog.resolve(name) {
            Resolution::Found(t) => t.clone(),
            _ => panic!("tool {name} missing from harness catalog"),
        }
    }

    fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        tool(name).execute(&args, ctx).unwrap()
    }

    fn kill_alive(target: &str) -> bool {
        Command::new("kill")
            .args(["-0", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn wait_for(f: impl Fn() -> bool, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        f()
    }

    fn rows(r: &ToolResult) -> Vec<Value> {
        r.data.clone().unwrap()["processes"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[test]
    fn process_start_stop_round_trip_with_log_under_harness_tmp() {
        let env = setup("pt-start-stop");
        let r = exec(
            "process_start",
            json!({ "command": "sleep 30", "name": "nap" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let id = data["id"].as_i64().unwrap();
        let pid = data["pid"].as_i64().unwrap();
        let log_path = PathBuf::from(data["logPath"].as_str().unwrap());
        assert!(log_path.starts_with(crate::paths::harness_processes_dir(&env.wd)));
        assert!(
            log_path.to_string_lossy().contains("Harness/tmp/processes"),
            "log not under Harness/tmp/processes: {log_path:?}"
        );
        assert!(log_path.exists(), "log file missing");
        assert!(kill_alive(&pid.to_string()), "sleep 30 must be alive");

        let r = exec("process_list", json!({}), &env.ctx);
        let list = rows(&r);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["status"], "running");
        assert_eq!(list[0]["pid"], pid);
        assert_eq!(list[0]["pgid"], pid);

        let r = exec("process_stop", json!({ "id": id }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["status"], "stopped");
        assert!(
            wait_for(|| !kill_alive(&pid.to_string()), 5),
            "process {pid} survived stop"
        );

        let r = exec("process_list", json!({}), &env.ctx);
        assert_eq!(rows(&r)[0]["status"], "stopped");
        std::fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn process_logs_tails_output_of_a_running_process() {
        let env = setup("pt-logs");
        let r = exec(
            "process_start",
            json!({ "command": "echo bg-hello; sleep 30" }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let data = r.data.unwrap();
        let id = data["id"].as_i64().unwrap();
        let pid = data["pid"].as_i64().unwrap();

        let seen = wait_for(
            || {
                let r = exec("process_logs", json!({ "id": id }), &env.ctx);
                r.ok && r.data.clone().unwrap()["log"].as_str().unwrap().contains("bg-hello")
            },
            5,
        );
        assert!(seen, "echo output never reached the log");

        let r = exec("process_logs", json!({ "id": id, "lines": 5 }), &env.ctx);
        let data = r.data.unwrap();
        assert_eq!(data["status"], "running");
        assert_eq!(data["id"], id);

        let r = exec("process_stop", json!({ "id": id }), &env.ctx);
        assert_eq!(r.data.unwrap()["status"], "stopped");
        assert!(wait_for(|| !kill_alive(&pid.to_string()), 5));
        std::fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn process_list_reconciles_exited_processes() {
        let env = setup("pt-reconcile");
        for cmd in ["exit 0", "exit 7"] {
            let r = exec("process_start", json!({ "command": cmd }), &env.ctx);
            assert!(r.ok, "{:?}", r.error);
        }
        let settled = wait_for(
            || {
                let r = exec("process_list", json!({}), &env.ctx);
                rows(&r).len() == 2 && rows(&r).iter().all(|p| p["status"] != "running")
            },
            5,
        );
        assert!(settled, "exited processes were not reconciled");

        let r = exec("process_list", json!({}), &env.ctx);
        let list = rows(&r);
        let by_cmd: std::collections::HashMap<String, Value> = list
            .iter()
            .map(|p| (p["command"].as_str().unwrap().to_string(), p.clone()))
            .collect();
        assert_eq!(by_cmd["exit 0"]["status"], "exited");
        assert_eq!(by_cmd["exit 0"]["exitCode"], 0);
        assert_eq!(by_cmd["exit 7"]["status"], "error");
        assert_eq!(by_cmd["exit 7"]["exitCode"], 7);
        std::fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn reconcile_leaves_foreign_live_pids_and_marks_dead_ones() {
        let env = setup("pt-stale");
        // A live pid that is not our child (this very test process) and a
        // pid we spawned and already reaped (dead): the startup-reconcile
        // semantics — never kill, only update status.
        let self_pid = std::process::id() as i64;
        let dead_child = Command::new("sleep").arg("0.05").spawn().unwrap();
        let dead_pid = dead_child.id() as i64;
        dead_child.wait_with_output().unwrap();

        with_conn(&env.ctx, |conn| {
            for (name, pid) in [("alive", self_pid), ("dead", dead_pid)] {
                conn.execute(
                    "INSERT INTO processes (name, pid, pgid, command, log_path, status, created_at, updated_at)
                     VALUES (?1, ?2, ?2, 'x', 'gone.log', 'running', datetime('now','localtime'), datetime('now','localtime'))",
                    params![name, pid],
                )?;
            }
            Ok(())
        })
        .unwrap();

        let r = exec("process_list", json!({}), &env.ctx);
        let list = rows(&r);
        let by_name: std::collections::HashMap<String, Value> = list
            .iter()
            .map(|p| (p["name"].as_str().unwrap().to_string(), p.clone()))
            .collect();
        assert_eq!(by_name["alive"]["status"], "running", "foreign live pid must stay running");
        assert_eq!(by_name["dead"]["status"], "exited");
        assert!(kill_alive(&self_pid.to_string()), "reconcile must not kill");
        std::fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn process_tool_classes_policies_and_schemas() {
        for name in ["process_logs", "process_list"] {
            assert_eq!(tool(name).class(), ToolClass::ReadOnly, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Skip, "{name} policy");
        }
        for name in ["process_start", "process_stop"] {
            assert_eq!(tool(name).class(), ToolClass::Mutating, "{name} class");
            assert_eq!(tool(name).policy(), PermissionPolicy::Check, "{name} policy");
            assert!(
                tool(name).danger_reason(&json!({}), &ToolCtx::default()).is_some(),
                "{name} danger_reason"
            );
        }
        // Schemas are strict: unknown properties and missing ids are rejected.
        assert!(validate_args(&tool("process_start").parameters(), &json!({ "bogus": 1 })).is_err());
        assert!(validate_args(&tool("process_logs").parameters(), &json!({})).is_err());
        assert!(validate_args(&tool("process_stop").parameters(), &json!({ "x": 1 })).is_err());
        assert!(validate_args(&tool("process_list").parameters(), &json!({})).is_ok());
        assert!(validate_args(
            &tool("process_start").parameters(),
            &json!({ "command": "sleep 1", "cwd": "x", "name": "n" })
        )
        .is_ok());
    }

    #[test]
    fn process_logs_unknown_id_is_not_found() {
        let env = setup("pt-unknown");
        for name in ["process_logs", "process_stop"] {
            let r = exec(name, json!({ "id": 999 }), &env.ctx);
            assert_eq!(r.error.unwrap().code, "not_found", "{name}");
        }
        std::fs::remove_dir_all(&env.wd).unwrap();
    }
}
