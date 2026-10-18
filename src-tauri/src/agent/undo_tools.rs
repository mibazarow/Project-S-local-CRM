// Undo tools (SPEC undo slice): expose the bounded undo stack to the agent.
// undo_last pops and reverses the newest mutation; undo_list previews the stack.
use super::project_tools::{envelope, require_wd, state_of, with_conn};
use crate::sync_util::lock_recover;
use super::result::ToolResult;
use super::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::Result;
use crate::undo;
use serde_json::{json, Value};

pub struct UndoLastTool;

impl Tool for UndoLastTool {
    fn name(&self) -> &str {
        "undo_last"
    }

    fn description(&self) -> &str {
        "Отменить последнее обратимое действие."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }

    fn class(&self) -> ToolClass {
        ToolClass::Mutating
    }

    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }

    fn execute(&self, _args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let wd = require_wd(ctx)?;
        let out = with_conn(ctx, |conn| undo::undo_last(conn, &wd))?;
        emit_undo_events(ctx, &out);
        Ok(envelope(Ok(out)))
    }
}

pub struct UndoListTool;

impl Tool for UndoListTool {
    fn name(&self) -> &str {
        "undo_list"
    }

    fn description(&self) -> &str {
        "Список последних отменяемых действий."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
            },
            "required": [],
            "additionalProperties": false
        })
    }

    fn class(&self) -> ToolClass {
        ToolClass::ReadOnly
    }

    fn policy(&self) -> PermissionPolicy {
        PermissionPolicy::Skip
    }

    fn execute(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolResult> {
        let limit = args.get("limit").and_then(Value::as_u64).map(|v| v as usize);
        Ok(envelope(with_conn(ctx, |conn| undo::list(conn, limit))))
    }
}

fn emit_undo_events(ctx: &ToolCtx, out: &undo::UndoOutcome) {
    let Ok(state) = state_of(ctx) else {
        return;
    };
    let binding = lock_recover(&state.emitter);
    let Some(emitter) = binding.as_ref() else {
        return;
    };
    if let Some(fs) = &out.fs_event {
        emitter.emit_event(
            "fs-changed",
            json!({ "projectId": fs.project_id, "relPath": fs.rel_path }),
        );
    }
    if let Some(rel) = &out.notes_rel_path {
        emitter.emit_event("notes-changed", json!({ "relPath": rel }));
    }
    if let Some(project_id) = out.task_project_id {
        emitter.emit_event("tasks-changed", json!({ "projectId": project_id }));
    }
    if out.trash_id.is_some() {
        emitter.emit_event("trash-changed", json!({ "id": out.trash_id }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use crate::reliability;
    use crate::state::{AppState, UiEmitter};
    use crate::tasks;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct RecordingEmitter {
        events: Mutex<Vec<(String, Value)>>,
    }

    impl UiEmitter for RecordingEmitter {
        fn emit_event(&self, event: &str, payload: Value) {
            self.events.lock().unwrap().push((event.to_string(), payload));
        }
    }

    impl RecordingEmitter {
        fn names(&self) -> Vec<String> {
            self.events.lock().unwrap().iter().map(|(n, _)| n.clone()).collect()
        }
    }

    struct Env {
        wd: PathBuf,
        state: Arc<AppState>,
        emitter: Arc<RecordingEmitter>,
    }

    fn setup(tag: &str) -> Env {
        let wd = temp_test_dir(tag);
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(db::open(&paths::db_path(&wd)).unwrap());
        let emitter = Arc::new(RecordingEmitter {
            events: Mutex::new(Vec::new()),
        });
        *state.emitter.lock().unwrap() = Some(Arc::clone(&emitter) as Arc<dyn UiEmitter>);
        Env { wd, state, emitter }
    }

    fn ctx_of(env: &Env) -> ToolCtx {
        ToolCtx {
            working_dir: Some(env.wd.clone()),
            state: Some(Arc::clone(&env.state)),
            ..Default::default()
        }
    }

    fn with_db<T>(env: &Env, f: impl FnOnce(&rusqlite::Connection) -> T) -> T {
        let guard = env.state.db.lock().unwrap();
        f(guard.as_ref().expect("test db installed"))
    }

    #[test]
    fn undo_tools_are_skip_policy_with_expected_classes() {
        assert_eq!(UndoLastTool.policy(), PermissionPolicy::Skip);
        assert_eq!(UndoLastTool.class(), ToolClass::Mutating);
        assert_eq!(UndoListTool.policy(), PermissionPolicy::Skip);
        assert_eq!(UndoListTool.class(), ToolClass::ReadOnly);
    }

    #[test]
    fn undo_last_tool_reverts_task_create_and_notifies_tasks_changed() {
        let env = setup("undo-tool-task");
        let project_id = with_db(&env, |conn| {
            let project = projects::create_project(conn, &env.wd, "example-ru").unwrap();
            let task = tasks::task_create(conn, project.id, "undo me", None).unwrap();
            undo::push_task_create(conn, task.id, project.id).unwrap();
            assert_eq!(tasks::tasks_list(conn, project.id).unwrap().len(), 1);
            project.id
        });
        let out = UndoLastTool.execute(&json!({}), &ctx_of(&env)).unwrap();
        assert!(out.ok, "{out:?}");
        assert_eq!(out.data.unwrap()["undoneKind"], "task_create");
        assert!(with_db(&env, |conn| tasks::tasks_list(conn, project_id).unwrap()).is_empty());
        assert_eq!(env.emitter.names(), ["tasks-changed"]);
        let payload = env.emitter.events.lock().unwrap()[0].1.clone();
        assert_eq!(payload["projectId"], project_id);
        // The stack is now empty: undo_last propagates the domain error as a
        // hard Err (execute uses `?`, so "nothing to undo" never becomes an
        // ok envelope).
        let err = UndoLastTool.execute(&json!({}), &ctx_of(&env)).unwrap_err();
        assert!(err.to_string().contains("nothing to undo"), "{err}");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn undo_last_tool_restores_written_file_and_emits_fs_changed() {
        let env = setup("undo-tool-write");
        let file = with_db(&env, |conn| {
            let project = projects::create_project(conn, &env.wd, "example-ru").unwrap();
            let root = projects::project_root(conn, &env.wd, project.id).unwrap();
            let file = root.join("a.txt");
            fs::write(&file, "old").unwrap();
            let version_id =
                reliability::version_save(conn, &env.wd, "project", &file.display().to_string(), "old")
                    .unwrap();
            fs::write(&file, "new").unwrap();
            undo::push_file_write(
                conn,
                "project",
                &file.display().to_string(),
                Some(version_id),
                false,
                Some(project.id),
                Some("a.txt"),
            )
            .unwrap();
            file
        });
        let out = UndoLastTool.execute(&json!({}), &ctx_of(&env)).unwrap();
        assert!(out.ok, "{out:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "old");
        assert_eq!(env.emitter.names(), ["fs-changed"]);
        let payload = env.emitter.events.lock().unwrap()[0].1.clone();
        assert_eq!(payload["relPath"], "a.txt");
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn undo_last_tool_restores_trashed_delete_and_emits_events() {
        let env = setup("undo-tool-delete");
        let file = with_db(&env, |conn| {
            let project = projects::create_project(conn, &env.wd, "example-ru").unwrap();
            let root = projects::project_root(conn, &env.wd, project.id).unwrap();
            let file = root.join("b.txt");
            fs::write(&file, "keep").unwrap();
            let trash_id =
                reliability::trash_put(conn, &env.wd, "project", "file", &file, None).unwrap();
            assert!(!file.exists());
            undo::push_delete(
                conn,
                trash_id,
                "project",
                &file.display().to_string(),
                Some(project.id),
                Some("b.txt"),
            )
            .unwrap();
            file
        });
        let out = UndoLastTool.execute(&json!({}), &ctx_of(&env)).unwrap();
        assert!(out.ok, "{out:?}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
        assert_eq!(env.emitter.names(), ["fs-changed", "trash-changed"]);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn undo_list_tool_previews_stack_newest_first_with_limit() {
        let env = setup("undo-tool-list");
        with_db(&env, |conn| {
            for i in 1..=3 {
                undo::push(conn, "task_create", json!({ "taskId": i, "projectId": 1 })).unwrap();
            }
        });
        let out = UndoListTool.execute(&json!({ "limit": 2 }), &ctx_of(&env)).unwrap();
        assert!(out.ok, "{out:?}");
        let entries = out.data.unwrap();
        assert_eq!(entries.as_array().unwrap().len(), 2);
        assert_eq!(entries[0]["description"], "task create: #3");
        assert_eq!(entries[1]["description"], "task create: #2");
        assert!(entries[0]["createdAt"].as_str().is_some());
        // No limit: the undo::list default (20) covers all three.
        let out = UndoListTool.execute(&json!({}), &ctx_of(&env)).unwrap();
        assert_eq!(out.data.unwrap().as_array().unwrap().len(), 3);
        // Listing never pops the stack.
        let out = UndoLastTool.execute(&json!({}), &ctx_of(&env)).unwrap();
        assert!(out.ok);
        fs::remove_dir_all(&env.wd).unwrap();
    }

    #[test]
    fn undo_tools_report_missing_run_context_as_errors() {
        // undo_last propagates the missing working dir as a hard error.
        let err = UndoLastTool.execute(&json!({}), &ToolCtx::default()).unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");
        // undo_list maps it into the error envelope through with_conn.
        let out = UndoListTool.execute(&json!({}), &ToolCtx::default()).unwrap();
        assert!(!out.ok);
        let err = out.error.unwrap();
        assert_eq!(err.code, "execution_error");
        assert!(err.message.contains("not configured"));
    }
}
