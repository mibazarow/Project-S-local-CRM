// Long-lived goals with steps (SPEC objectives slice): an objective is a
// global goal (not bound to a project) with an ordered checklist of steps and
// a status. Mutations emit "objectives-changed" so the Today panel reloads,
// whether the change came from a command or an agent tool.
use crate::agent::project_tools::{envelope, state_of, with_conn};
use crate::agent::result::{ToolErrorCode, ToolResult};
use crate::agent::tool::{PermissionPolicy, Tool, ToolClass, ToolCtx};
use crate::error::{AppError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const STATUSES: [&str; 4] = ["active", "paused", "done", "error"];
const MAX_STEPS: usize = 50;
const MAX_STEP_CHARS: usize = 500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectiveStepDto {
    pub id: i64,
    pub text: String,
    pub done: bool,
    pub position: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectiveDto {
    pub id: i64,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub steps: Vec<ObjectiveStepDto>,
    pub done_steps: i64,
    pub total_steps: i64,
}

// Payload of the "objectives-changed" event: the UI reloads the list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectivesChanged {
    pub id: Option<i64>,
}

// Distinguishes an explicit null (Some(None) -> clear) from an absent field (None -> keep).
mod opt_opt_str {
    use serde::de::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(Some)
    }
}

// Absent field leaves the value unchanged; explicit null clears the description.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ObjectivePatch {
    pub title: Option<String>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub description: Option<Option<String>>,
    pub status: Option<String>,
}

fn check_status(status: &str) -> Result<()> {
    if STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown objective status: {status}"
        )))
    }
}

fn check_steps(steps: &[String]) -> Result<()> {
    if steps.len() > MAX_STEPS {
        return Err(AppError::Invalid(format!(
            "too many objective steps: {} (max {MAX_STEPS})",
            steps.len()
        )));
    }
    for (i, step) in steps.iter().enumerate() {
        let text = step.trim();
        if text.is_empty() {
            return Err(AppError::Invalid(format!(
                "objective step {} has empty text",
                i + 1
            )));
        }
        if text.chars().count() > MAX_STEP_CHARS {
            return Err(AppError::Invalid(format!(
                "objective step {} is over {MAX_STEP_CHARS} characters",
                i + 1
            )));
        }
    }
    Ok(())
}

fn steps_of(conn: &Connection, objective_id: i64) -> Result<Vec<ObjectiveStepDto>> {
    let mut stmt = conn.prepare(
        "SELECT id, text, done, position FROM objective_steps
         WHERE objective_id = ?1 ORDER BY position, id",
    )?;
    let rows = stmt.query_map(params![objective_id], |row| {
        Ok(ObjectiveStepDto {
            id: row.get(0)?,
            text: row.get(1)?,
            done: row.get::<_, i64>(2)? != 0,
            position: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn dto(conn: &Connection, id: i64) -> Result<ObjectiveDto> {
    let (title, description, status, created_at, updated_at) = conn
        .query_row(
            "SELECT title, description, status, created_at, updated_at
             FROM objectives WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .map_err(|_| AppError::Invalid(format!("objective {id} not found")))?;
    let steps = steps_of(conn, id)?;
    let done_steps = steps.iter().filter(|s| s.done).count() as i64;
    let total_steps = steps.len() as i64;
    Ok(ObjectiveDto {
        id,
        title,
        description,
        status,
        created_at,
        updated_at,
        steps,
        done_steps,
        total_steps,
    })
}

pub fn objective_create(
    conn: &Connection,
    raw_title: &str,
    description: Option<String>,
    steps: &[String],
) -> Result<ObjectiveDto> {
    let title = raw_title.trim();
    if title.is_empty() {
        return Err(AppError::Invalid("objective title is empty".into()));
    }
    check_steps(steps)?;
    // An empty description is stored as NULL.
    let description = description
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());
    conn.execute(
        "INSERT INTO objectives (title, description, created_at, updated_at)
         VALUES (?1, ?2, datetime('now','localtime'), datetime('now','localtime'))",
        params![title, description],
    )?;
    let id = conn.last_insert_rowid();
    for (i, step) in steps.iter().enumerate() {
        conn.execute(
            "INSERT INTO objective_steps (objective_id, text, position) VALUES (?1, ?2, ?3)",
            params![id, step.trim(), (i + 1) as i64],
        )?;
    }
    dto(conn, id)
}

// Active first, then paused, then error, done last; newest first inside a status.
pub fn objective_list(conn: &Connection) -> Result<Vec<ObjectiveDto>> {
    let mut objectives: Vec<ObjectiveDto> = {
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, created_at, updated_at FROM objectives
             ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'paused' THEN 1 WHEN 'error' THEN 2 ELSE 3 END, id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ObjectiveDto {
                id: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                steps: Vec::new(),
                done_steps: 0,
                total_steps: 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let index: std::collections::HashMap<i64, usize> = objectives
        .iter()
        .enumerate()
        .map(|(i, o)| (o.id, i))
        .collect();
    let mut stmt = conn.prepare(
        "SELECT objective_id, id, text, done, position FROM objective_steps
         ORDER BY objective_id, position, id",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let objective_id: i64 = row.get(0)?;
        let step = ObjectiveStepDto {
            id: row.get(1)?,
            text: row.get(2)?,
            done: row.get::<_, i64>(3)? != 0,
            position: row.get(4)?,
        };
        // Steps always belong to a listed objective (FK + cascade), but stay
        // defensive: an orphan row must not panic the list.
        if let Some(&i) = index.get(&objective_id) {
            let objective = &mut objectives[i];
            objective.total_steps += 1;
            if step.done {
                objective.done_steps += 1;
            }
            objective.steps.push(step);
        }
    }
    Ok(objectives)
}

pub fn objective_update(conn: &Connection, id: i64, patch: ObjectivePatch) -> Result<ObjectiveDto> {
    let (mut title, mut description, mut status) = conn
        .query_row(
            "SELECT title, description, status FROM objectives WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| AppError::Invalid(format!("objective {id} not found")))?;
    if let Some(new_title) = patch.title {
        let new_title = new_title.trim().to_owned();
        if new_title.is_empty() {
            return Err(AppError::Invalid("objective title is empty".into()));
        }
        title = new_title;
    }
    if let Some(new_description) = patch.description {
        description = new_description
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty());
    }
    if let Some(new_status) = patch.status {
        check_status(&new_status)?;
        status = new_status;
    }
    conn.execute(
        "UPDATE objectives SET title = ?1, description = ?2, status = ?3,
                updated_at = datetime('now','localtime')
         WHERE id = ?4",
        params![title, description, status, id],
    )?;
    dto(conn, id)
}

// Marks one step done/undone; the step must belong to the objective.
pub fn objective_step_done(
    conn: &Connection,
    objective_id: i64,
    step_id: i64,
    done: bool,
) -> Result<ObjectiveDto> {
    let changed = conn.execute(
        "UPDATE objective_steps SET done = ?1 WHERE id = ?2 AND objective_id = ?3",
        params![done as i64, step_id, objective_id],
    )?;
    if changed == 0 {
        return Err(AppError::Invalid(format!(
            "step {step_id} not found for objective {objective_id}"
        )));
    }
    dto(conn, objective_id)
}

// Steps cascade with the objective (ON DELETE CASCADE, FKs on in db::open).
pub fn objective_delete(conn: &Connection, id: i64) -> Result<()> {
    let changed = conn.execute("DELETE FROM objectives WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AppError::Invalid(format!("objective {id} not found")));
    }
    Ok(())
}

// Best-effort UI notify through the run state's emitter (same path as
// todowrite's "todos-changed"); a missing emitter is not an error.
fn emit_changed(ctx: &ToolCtx, id: Option<i64>) {
    if let Ok(state) = state_of(ctx) {
        if let Some(emitter) = state.emitter.lock().unwrap().as_ref() {
            let payload = serde_json::to_value(ObjectivesChanged { id })
                .unwrap_or(Value::Null);
            emitter.emit_event("objectives-changed", payload);
        }
    }
}

// Domain validation failures (AppError::Invalid) are argument problems for
// the model; anything else keeps the generic envelope mapping.
fn invalid_as_args_error(e: AppError) -> ToolResult {
    match e {
        AppError::Invalid(m) => ToolResult::err(ToolErrorCode::InvalidArgs, m),
        other => envelope(Err::<ObjectiveDto, AppError>(other)),
    }
}

pub struct ObjectiveCreateTool;

impl Tool for ObjectiveCreateTool {
    fn name(&self) -> &str {
        "objective_create"
    }
    fn description(&self) -> &str {
        "Create a long-lived goal (objective) with an optional ordered list of steps. Use it for work that spans multiple sessions; the user sees goals with their progress in the Today panel."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "description": { "type": "string" },
                "steps": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["title"],
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
        let title = args["title"].as_str().unwrap_or_default();
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let steps: Vec<String> = args["steps"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let r = with_conn(ctx, |conn| objective_create(conn, title, description, &steps));
        match r {
            Ok(dto) => {
                emit_changed(ctx, Some(dto.id));
                Ok(envelope(Ok(dto)).capped(self.max_result_len()))
            }
            Err(e) => Ok(invalid_as_args_error(e)),
        }
    }
}

pub struct ObjectiveListTool;

impl Tool for ObjectiveListTool {
    fn name(&self) -> &str {
        "objective_list"
    }
    fn description(&self) -> &str {
        "List all goals (objectives) with their steps and progress: id, title, description, status, steps [{id, text, done}], doneSteps, totalSteps. Active goals come first."
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
        let r = with_conn(ctx, objective_list);
        match r {
            Ok(list) => Ok(envelope(Ok(list)).capped(self.max_result_len())),
            Err(e) => Ok(envelope(Err::<Vec<ObjectiveDto>, AppError>(e))),
        }
    }
}

pub struct ObjectiveUpdateTool;

impl Tool for ObjectiveUpdateTool {
    fn name(&self) -> &str {
        "objective_update"
    }
    fn description(&self) -> &str {
        "Update an objective: title, description (null clears) and/or status (active, paused, done, error). Absent fields stay unchanged."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "number" },
                "title": { "type": "string" },
                "description": { "type": ["string", "null"] },
                "status": { "type": "string", "enum": STATUSES }
            },
            "required": ["id"],
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
        let Some(id) = args.get("id").and_then(Value::as_i64) else {
            return Ok(ToolResult::err(ToolErrorCode::InvalidArgs, "id is required"));
        };
        let patch = ObjectivePatch {
            title: args.get("title").and_then(Value::as_str).map(str::to_owned),
            description: match args.get("description") {
                Some(Value::Null) => Some(None),
                Some(value) => value.as_str().map(|s| Some(s.to_owned())),
                None => None,
            },
            status: args.get("status").and_then(Value::as_str).map(str::to_owned),
        };
        let r = with_conn(ctx, |conn| objective_update(conn, id, patch));
        match r {
            Ok(dto) => {
                emit_changed(ctx, Some(dto.id));
                Ok(envelope(Ok(dto)).capped(self.max_result_len()))
            }
            Err(e) => Ok(invalid_as_args_error(e)),
        }
    }
}

pub struct ObjectiveStepDoneTool;

impl Tool for ObjectiveStepDoneTool {
    fn name(&self) -> &str {
        "objective_step_done"
    }
    fn description(&self) -> &str {
        "Mark one step of an objective done or undone (step ids come from objective_list)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objectiveId": { "type": "number" },
                "stepId": { "type": "number" },
                "done": { "type": "boolean" }
            },
            "required": ["objectiveId", "stepId", "done"],
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
        let objective_id = args.get("objectiveId").and_then(Value::as_i64);
        let step_id = args.get("stepId").and_then(Value::as_i64);
        let done = args.get("done").and_then(Value::as_bool);
        let (objective_id, step_id, done) = match (objective_id, step_id, done) {
            (Some(o), Some(s), Some(d)) => (o, s, d),
            _ => {
                return Ok(ToolResult::err(
                    ToolErrorCode::InvalidArgs,
                    "objectiveId, stepId and done are required",
                ))
            }
        };
        let r = with_conn(ctx, |conn| objective_step_done(conn, objective_id, step_id, done));
        match r {
            Ok(dto) => {
                emit_changed(ctx, Some(dto.id));
                Ok(envelope(Ok(dto)).capped(self.max_result_len()))
            }
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

    // A run state with a migrated DB (FKs on) and a recording emitter.
    fn setup(tag: &str) -> Env {
        let wd = temp_test_dir(tag);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let state = Arc::new(AppState::new());
        *state.working_dir.lock().unwrap() = Some(wd.clone());
        *state.db.lock().unwrap() = Some(conn);
        let recorder = RecordingEmitter::default();
        let events = recorder.events.clone();
        *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
        let ctx = ToolCtx {
            project_id: None,
            session_id: None,
            chat_id: None,
            working_dir: Some(wd.clone()),
            state: Some(state),
            notes_root: None,
        };
        Env { wd, ctx, events }
    }

    fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
        let tool: &dyn Tool = match name {
            "objective_create" => &ObjectiveCreateTool,
            "objective_list" => &ObjectiveListTool,
            "objective_update" => &ObjectiveUpdateTool,
            "objective_step_done" => &ObjectiveStepDoneTool,
            _ => panic!("unknown tool {name}"),
        };
        tool.execute(&args, ctx).unwrap()
    }

    fn db_conn(env: &Env) -> std::sync::MutexGuard<'_, Option<Connection>> {
        env.ctx.state.as_ref().unwrap().db.lock().unwrap()
    }

    #[test]
    fn crud_steps_and_progress_round_trip() {
        let env = setup("objectives-crud");
        let guard = db_conn(&env);
        let conn = guard.as_ref().unwrap();

        let created = objective_create(
            conn,
            "  Запустить сайт  ",
            Some("  ".to_owned()),
            &["Собрать макет".to_owned(), "Сверстать".to_owned()],
        )
        .unwrap();
        assert_eq!(created.title, "Запустить сайт", "title must be trimmed");
        assert_eq!(created.description, None, "blank description stores NULL");
        assert_eq!(created.status, "active");
        assert_eq!(created.total_steps, 2);
        assert_eq!(created.done_steps, 0);
        let positions: Vec<i64> = created.steps.iter().map(|s| s.position).collect();
        assert_eq!(positions, vec![1, 2], "positions must be 1..N");

        let step_id = created.steps[0].id;
        objective_step_done(conn, created.id, step_id, true).unwrap();
        let listed = objective_list(conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].done_steps, listed[0].total_steps), (1, 2));
        assert!(listed[0].steps[0].done);

        let updated = objective_update(
            conn,
            created.id,
            ObjectivePatch {
                title: Some("Запустить сайт v2".to_owned()),
                description: Some(Some("по этапам".to_owned())),
                status: Some("paused".to_owned()),
            },
        )
        .unwrap();
        assert_eq!(updated.title, "Запустить сайт v2");
        assert_eq!(updated.description.as_deref(), Some("по этапам"));
        assert_eq!(updated.status, "paused");

        // Absent patch fields keep the current values.
        let kept = objective_update(conn, created.id, ObjectivePatch::default()).unwrap();
        assert_eq!((kept.title.as_str(), kept.status.as_str()), ("Запустить сайт v2", "paused"));
        // Explicit null clears the description.
        let cleared = objective_update(
            conn,
            created.id,
            ObjectivePatch {
                description: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cleared.description, None);

        for bad in ["bogus", "new", ""] {
            let err = objective_update(
                conn,
                created.id,
                ObjectivePatch {
                    status: Some(bad.to_owned()),
                    ..Default::default()
                },
            );
            assert!(err.is_err(), "status {bad:?} accepted");
        }
        assert!(objective_update(conn, 999, ObjectivePatch::default()).is_err());
        assert!(objective_step_done(conn, created.id, 999, true).is_err());
        fs_cleanup(&env.wd);
    }

    #[test]
    fn delete_cascades_to_steps() {
        let env = setup("objectives-cascade");
        let guard = db_conn(&env);
        let conn = guard.as_ref().unwrap();
        let created = objective_create(
            conn,
            "g",
            None,
            &["a".to_owned(), "b".to_owned()],
        )
        .unwrap();
        objective_delete(conn, created.id).unwrap();
        assert!(objective_list(conn).unwrap().is_empty());
        let steps: i64 = conn
            .query_row(
                "SELECT count(*) FROM objective_steps WHERE objective_id = ?1",
                params![created.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(steps, 0, "steps survived the objective delete");
        assert!(objective_delete(conn, created.id).is_err(), "second delete accepted");
        fs_cleanup(&env.wd);
    }

    #[test]
    fn validation_rejects_empty_title_and_bad_steps() {
        let env = setup("objectives-valid");
        let guard = db_conn(&env);
        let conn = guard.as_ref().unwrap();
        assert!(objective_create(conn, "  ", None, &[]).is_err());
        assert!(objective_create(conn, "g", None, &["  ".to_owned()]).is_err());
        assert!(objective_create(conn, "g", None, &["x".repeat(MAX_STEP_CHARS + 1)]).is_err());
        let many: Vec<String> = (0..=MAX_STEPS).map(|i| format!("step {i}")).collect();
        assert!(objective_create(conn, "g", None, &many).is_err());
        fs_cleanup(&env.wd);
    }

    #[test]
    fn list_orders_active_first_and_done_last() {
        let env = setup("objectives-order");
        let guard = db_conn(&env);
        let conn = guard.as_ref().unwrap();
        let done = objective_create(conn, "done-goal", None, &[]).unwrap();
        objective_update(
            conn,
            done.id,
            ObjectivePatch {
                status: Some("done".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let paused = objective_create(conn, "paused-goal", None, &[]).unwrap();
        objective_update(
            conn,
            paused.id,
            ObjectivePatch {
                status: Some("paused".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        objective_create(conn, "active-goal", None, &[]).unwrap();
        let statuses: Vec<String> = objective_list(conn)
            .unwrap()
            .into_iter()
            .map(|o| o.status)
            .collect();
        assert_eq!(statuses, vec!["active", "paused", "done"]);
        fs_cleanup(&env.wd);
    }

    #[test]
    fn tools_run_through_the_agent_and_emit_objectives_changed() {
        let env = setup("objectives-tools");
        let r = exec(
            "objective_create",
            json!({ "title": "цель", "steps": ["шаг 1", "шаг 2"] }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        let created = r.data.unwrap();
        let objective_id = created["id"].as_i64().unwrap();
        assert_eq!(created["doneSteps"], json!(0));
        assert_eq!(created["totalSteps"], json!(2));

        let step_id = created["steps"][0]["id"].as_i64().unwrap();
        let r = exec(
            "objective_step_done",
            json!({ "objectiveId": objective_id, "stepId": step_id, "done": true }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["doneSteps"], json!(1));

        let r = exec(
            "objective_update",
            json!({ "id": objective_id, "status": "done", "description": null }),
            &env.ctx,
        );
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["status"], json!("done"));

        let r = exec("objective_list", json!({}), &env.ctx);
        assert!(r.ok, "{:?}", r.error);
        // envelope serializes the Vec directly: data is the objectives array
        let list = r.data.unwrap();
        let objectives = list.as_array().expect("list data must be an array");
        assert_eq!(objectives.len(), 1);
        assert_eq!(objectives[0]["doneSteps"], json!(1));

        let events = env.events.lock().unwrap().clone();
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["objectives-changed"; 3]);
        assert_eq!(events[0].1, json!({ "id": objective_id }));
        fs_cleanup(&env.wd);
    }

    #[test]
    fn tool_argument_errors_are_invalid_args() {
        let env = setup("objectives-tool-args");
        let r = exec("objective_create", json!({ "title": "  " }), &env.ctx);
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        let r = exec("objective_update", json!({}), &env.ctx);
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        let r = exec("objective_step_done", json!({ "objectiveId": 1 }), &env.ctx);
        assert_eq!(r.error.as_ref().unwrap().code, "invalid_args");
        let r = exec(
            "objective_update",
            json!({ "id": 999, "status": "done" }),
            &env.ctx,
        );
        assert!(r.error.as_ref().unwrap().message.contains("not found"));
        fs_cleanup(&env.wd);
    }

    #[test]
    fn tool_classes_and_policies_match_the_spec() {
        let expected: [(&dyn Tool, ToolClass, PermissionPolicy); 4] = [
            (&ObjectiveCreateTool, ToolClass::Mutating, PermissionPolicy::Skip),
            (&ObjectiveListTool, ToolClass::ReadOnly, PermissionPolicy::Skip),
            (&ObjectiveUpdateTool, ToolClass::Mutating, PermissionPolicy::Skip),
            (&ObjectiveStepDoneTool, ToolClass::Mutating, PermissionPolicy::Skip),
        ];
        for (tool, class, policy) in expected {
            assert_eq!(tool.class(), class, "{} class", tool.name());
            assert_eq!(tool.policy(), policy, "{} policy", tool.name());
        }
    }

    fn fs_cleanup(wd: &PathBuf) {
        let _ = std::fs::remove_dir_all(wd);
    }
}
