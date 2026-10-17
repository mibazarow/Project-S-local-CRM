// Per-conversation TODO plans (SPEC: todowrite/todoread + read-only UI panel).
// One ordered list per scope: a project chat owns the 'project' plan keyed by
// project id, a harness session owns its 'session' plan keyed by session id
// (same project/session id separation as RunKey). todowrite replaces the whole
// list; the UI panel reloads on the "todos-changed" event.
use crate::agent::project_tools::{envelope, state_of, with_conn};
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};

pub const SCOPE_PROJECT: &str = "project";
pub const SCOPE_SESSION: &str = "session";
const MAX_ITEMS: usize = 50;
const MAX_ITEM_CHARS: usize = 500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoDto {
    pub id: i64,
    pub text: String,
    pub done: bool,
    pub position: i64,
}

// Payload of the "todos-changed" event: the UI reloads the matching scope.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodosChanged {
    pub scope_kind: String,
    pub scope_id: i64,
}

// One parsed todowrite item; validation lives in todos_replace.
pub struct TodoItemIn {
    pub text: String,
    pub done: bool,
}

fn check_scope_kind(kind: &str) -> Result<()> {
    if kind == SCOPE_PROJECT || kind == SCOPE_SESSION {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown todo scope kind: {kind}"
        )))
    }
}

// The plan scope of an agent run: project chats plan for the project, harness
// sessions for the session; a run bound to neither has no plan to write.
pub fn scope_of(ctx: &ToolCtx) -> Result<(String, i64)> {
    if let Some(id) = ctx.project_id {
        Ok((SCOPE_PROJECT.to_owned(), id))
    } else if let Some(id) = ctx.session_id {
        Ok((SCOPE_SESSION.to_owned(), id))
    } else {
        Err(AppError::Invalid(
            "no project or session bound to this agent run".into(),
        ))
    }
}

pub fn todos_list(conn: &Connection, scope_kind: &str, scope_id: i64) -> Result<Vec<TodoDto>> {
    check_scope_kind(scope_kind)?;
    let mut stmt = conn.prepare(
        "SELECT id, text, done, position FROM todos
         WHERE scope_kind = ?1 AND scope_id = ?2 ORDER BY position, id",
    )?;
    let rows = stmt.query_map(params![scope_kind, scope_id], |row| {
        Ok(TodoDto {
            id: row.get(0)?,
            text: row.get(1)?,
            done: row.get::<_, i64>(2)? != 0,
            position: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// Replaces the whole list for a scope: validate first, then delete + reinsert
// so positions are exactly 1..N in the order the model sent. An empty items
// list clears the plan.
pub fn todos_replace(
    conn: &Connection,
    scope_kind: &str,
    scope_id: i64,
    items: &[TodoItemIn],
) -> Result<Vec<TodoDto>> {
    check_scope_kind(scope_kind)?;
    if items.len() > MAX_ITEMS {
        return Err(AppError::Invalid(format!(
            "too many todo items: {} (max {MAX_ITEMS})",
            items.len()
        )));
    }
    for (i, item) in items.iter().enumerate() {
        let text = item.text.trim();
        if text.is_empty() {
            return Err(AppError::Invalid(format!("todo item {} has empty text", i + 1)));
        }
        if text.chars().count() > MAX_ITEM_CHARS {
            return Err(AppError::Invalid(format!(
                "todo item {} is over {MAX_ITEM_CHARS} characters",
                i + 1
            )));
        }
    }
    conn.execute(
        "DELETE FROM todos WHERE scope_kind = ?1 AND scope_id = ?2",
        params![scope_kind, scope_id],
    )?;
    for (i, item) in items.iter().enumerate() {
        conn.execute(
            "INSERT INTO todos (scope_kind, scope_id, text, done, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now','localtime'), datetime('now','localtime'))",
            params![
                scope_kind,
                scope_id,
                item.text.trim(),
                item.done as i64,
                (i + 1) as i64
            ],
        )?;
    }
    todos_list(conn, scope_kind, scope_id)
}

// Best-effort UI notify through the run state's emitter (same path as
// rename_chat's "sessions-changed"); a missing emitter is not an error.
fn emit_changed(ctx: &ToolCtx, scope_kind: &str, scope_id: i64) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = state.emitter.lock().unwrap().as_ref() {
            let payload = serde_json::to_value(TodosChanged {
                scope_kind: scope_kind.to_owned(),
                scope_id,
            })
            .unwrap_or(Value::Null);
            emitter.emit_event("todos-changed", payload);
        }
    }
}

// Model-facing shape: exactly [{text, done}] in list order.
fn public_items(list: &[TodoDto]) -> Vec<Value> {
    list.iter()
        .map(|t| json!({ "text": t.text, "done": t.done }))
        .collect()
}

// Domain validation failures (AppError::Invalid) are argument problems for
// the model; anything else keeps the generic envelope mapping.
fn invalid_as_args_error(e: AppError) -> ToolResult {
    match e {
        AppError::Invalid(m) => ToolResult::err(ToolErrorCode::InvalidArgs, m),
        other => envelope(Err::<Vec<TodoDto>, AppError>(other)),
    }
}

pub struct TodoWriteTool;

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todowrite"
    }
    fn description(&self) -> &str {
        "Replace the plan (TODO list) of this conversation. items is the full ordered list, each {text, done?}; absent done means not done, an empty items array clears the plan. The user sees the plan above the chat, so keep it current while working."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" },
                            "done": { "type": "boolean" }
                        },
                        "required": ["text"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items"],
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
        let (scope_kind, scope_id) = match scope_of(ctx) {
            Ok(scope) => scope,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string())),
        };
        let items: Vec<TodoItemIn> = args["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|item| TodoItemIn {
                text: item.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
                done: item.get("done").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect();
        let r = with_conn(ctx, |conn| {
            todos_replace(conn, &scope_kind, scope_id, &items)
        });
        match r {
            Ok(list) => {
                emit_changed(ctx, &scope_kind, scope_id);
                Ok(ToolResult::ok(json!({ "items": public_items(&list) }))
                    .capped(self.max_result_len()))
            }
            Err(e) => Ok(invalid_as_args_error(e)),
        }
    }
}

pub struct TodoReadTool;

impl Tool for TodoReadTool {
    fn name(&self) -> &str {
        "todoread"
    }
    fn description(&self) -> &str {
        "Read the current plan (TODO list) of this conversation as [{text, done}] in list order."
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
        let (scope_kind, scope_id) = match scope_of(ctx) {
            Ok(scope) => scope,
            Err(e) => return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, e.to_string())),
        };
        let r = with_conn(ctx, |conn| todos_list(conn, &scope_kind, scope_id));
        match r {
            Ok(list) => Ok(ToolResult::ok(json!({ "items": public_items(&list) }))
                .capped(self.max_result_len())),
            Err(e) => Ok(invalid_as_args_error(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tool::Tool;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::state::{AppState, UiEmitter};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingEmitter {
        events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl UiEmitter for RecordingEmitter {
        fn emit_event(&self, event: &str, payload: Value) {
            self.events
                .lock()
                .unwrap()
                .push((event.to_owned(), payload));
        }
    }

    struct Env {
        wd: PathBuf,
        ctx: ToolCtx,
        events: Arc<Mutex<Vec<(String, Value)>>>,
    }

    // A run state with a migrated DB and a recording emitter; the scope ids
    // themselves are not FK-checked here, the tools only key by them.
    fn setup(tag: &str, project_id: Option<i64>, session_id: Option<i64>) -> Env {
        let wd = temp_test_dir(tag);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let recorder = RecordingEmitter::default();
        let events = recorder.events.clone();
        *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
        let ctx = ToolCtx {
            project_id,
            session_id,
            working_dir: Some(wd.clone()),
            state: Some(state),
        };
        Env { wd, ctx, events }
    }

    fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        let tool: &dyn Tool = if name == "todowrite" {
            &TodoWriteTool
        } else {
            &TodoReadTool
        };
        tool.execute(&args, ctx).unwrap()
    }

    fn db_list(env: &Env, kind: &str, id: i64) -> Vec<TodoDto> {
        let state = env.ctx.state.as_ref().unwrap();
        let guard = state.db.lock().unwrap();
        todos_list(guard.as_ref().unwrap(), kind, id).unwrap()
    }

    #[test]
    fn todowrite_replaces_the_whole_list_and_read_returns_it() {
        let env = setup("todos-replace", Some(1), None);
        let r = exec(
            "todowrite",
            json!({ "items": [{ "text": "a" }, { "text": "b", "done": true }, { "text": "c" }] }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap()["items"],
            json!([
                { "text": "a", "done": false },
                { "text": "b", "done": true },
                { "text": "c", "done": false }
            ])
        );
        let stored = db_list(&env, SCOPE_PROJECT, 1);
        let positions: Vec<i64> = stored.iter().map(|t| t.position).collect();
        assert_eq!(positions, vec![1, 2, 3], "positions must be 1..N");

        // replace drops everything not in the new list
        let r = exec("todowrite", json!({ "items": [{ "text": "only" }] }), &env.ctx);
        assert!(r.ok);
        let r = exec("todoread", json!({}), &env.ctx);
        assert_eq!(r.data.unwrap()["items"], json!([{ "text": "only", "done": false }]));

        // empty items clears the plan
        let r = exec("todowrite", json!({ "items": [] }), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        assert!(db_list(&env, SCOPE_PROJECT, 1).is_empty());
        fs_cleanup(&env.wd);
    }

    #[test]
    fn scopes_are_isolated_between_projects_and_sessions() {
        let proj1 = setup("todos-p1", Some(1), None);
        let proj2 = setup("todos-p2", Some(2), None);
        let sess = setup("todos-s1", None, Some(1));
        exec("todowrite", json!({ "items": [{ "text": "p1" }] }), &proj1.ctx);
        exec("todowrite", json!({ "items": [{ "text": "p2" }] }), &proj2.ctx);
        exec("todowrite", json!({ "items": [{ "text": "s1" }] }), &sess.ctx);
        assert_eq!(db_list(&proj1, SCOPE_PROJECT, 1)[0].text, "p1");
        assert_eq!(db_list(&proj2, SCOPE_PROJECT, 2)[0].text, "p2");
        // session id 1 must not see project id 1's plan
        assert_eq!(db_list(&sess, SCOPE_SESSION, 1)[0].text, "s1");
        let r = exec("todoread", json!({}), &sess.ctx);
        assert_eq!(r.data.unwrap()["items"], json!([{ "text": "s1", "done": false }]));
        fs_cleanup(&proj1.wd);
        fs_cleanup(&proj2.wd);
        fs_cleanup(&sess.wd);
    }

    #[test]
    fn validation_rejects_empty_oversized_and_too_many_items() {
        let env = setup("todos-valid", Some(1), None);
        let r = exec("todowrite", json!({ "items": [{ "text": "  " }] }), &env.ctx);
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        let r = exec(
            "todowrite",
            json!({ "items": [{ "text": "x".repeat(MAX_ITEM_CHARS + 1) }] }),
            &env.ctx,
        );
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        let many: Vec<Value> = (0..=MAX_ITEMS)
            .map(|i| json!({ "text": format!("item {i}") }))
            .collect();
        let r = exec("todowrite", json!({ "items": many }), &env.ctx);
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        // a failed replace keeps the previous plan intact
        exec("todowrite", json!({ "items": [{ "text": "kept" }] }), &env.ctx);
        assert_eq!(db_list(&env, SCOPE_PROJECT, 1)[0].text, "kept");
        // surrounding whitespace is trimmed on store
        exec("todowrite", json!({ "items": [{ "text": "  spaced  " }] }), &env.ctx);
        assert_eq!(db_list(&env, SCOPE_PROJECT, 1)[0].text, "spaced");
        fs_cleanup(&env.wd);
    }

    #[test]
    fn run_without_project_or_session_gets_a_clear_error() {
        let env = setup("todos-noscope", None, None);
        for name in ["todowrite", "todoread"] {
            let r = exec(name, json!({ "items": [] }), &env.ctx);
            assert!(!r.ok, "{name} accepted an unbound run");
            assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
            assert!(
                r.error.unwrap().message.contains("no project or session"),
                "{name} message"
            );
        }
        fs_cleanup(&env.wd);
    }

    #[test]
    fn todowrite_emits_todos_changed_for_its_scope() {
        let env = setup("todos-emit", None, Some(7));
        exec("todowrite", json!({ "items": [{ "text": "a" }] }), &env.ctx);
        let events = env.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![(
                "todos-changed".to_owned(),
                json!({ "scopeKind": "session", "scopeId": 7 })
            )]
        );
        fs_cleanup(&env.wd);
    }

    #[test]
    fn unknown_scope_kind_is_rejected_by_the_domain() {
        let env = setup("todos-kind", Some(1), None);
        let guard = env.ctx.state.as_ref().unwrap().db.lock().unwrap();
        let conn = guard.as_ref().unwrap();
        assert!(todos_list(conn, "bogus", 1).is_err());
        assert!(todos_replace(conn, "bogus", 1, &[] as &[TodoItemIn]).is_err());
        fs_cleanup(&env.wd);
    }

    fn fs_cleanup(wd: &PathBuf) {
        let _ = std::fs::remove_dir_all(wd);
    }
}
