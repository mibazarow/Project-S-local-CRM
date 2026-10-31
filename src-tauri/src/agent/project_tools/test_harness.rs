// Shared test scaffolding for the project tool submodules: a temp project with
// a bound ToolCtx, a recording UI emitter, and catalog lookup/exec helpers.
use super::super::catalog::Resolution;
use crate::agent::tool::{Tool, ToolCtx};
use crate::agent::result::ToolResult;
use crate::db;
use crate::onboarding::temp_test_dir;
use crate::paths;
use crate::projects;
use crate::state::{AppState, UiEmitter};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
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

pub struct Env {
    pub wd: PathBuf,
    pub ctx: ToolCtx,
    pub events: Arc<Mutex<Vec<(String, Value)>>>,
}

pub fn setup(tag: &str) -> Env {
    let wd = temp_test_dir(tag);
    fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
    let conn = db::open(&paths::db_path(&wd)).unwrap();
    let dto = projects::create_project(&conn, &wd, "proj").unwrap();
    let state = Arc::new(AppState::new());
    *state.working_dir.lock().unwrap() = Some(wd.clone());
    *state.db.lock().unwrap() = Some(conn);
    let recorder = RecordingEmitter::default();
    let events = recorder.events.clone();
    *state.emitter.lock().unwrap() = Some(Arc::new(recorder));
    let ctx = ToolCtx {
        scope: None,
        cancel: None,
        project_id: Some(dto.id),
        session_id: None,
        chat_id: None,
        working_dir: Some(wd.clone()),
        state: Some(state),
        notes_root: None,
    };
    Env { wd, ctx, events }
}

pub fn root(wd: &Path) -> PathBuf {
    paths::projects_dir(wd).join("proj")
}

pub fn tool(name: &str) -> Arc<dyn Tool> {
    let catalog = super::project_catalog();
    match catalog.resolve(name) {
        Resolution::Found(t) => t.clone(),
        _ => panic!("tool {name} missing from project catalog"),
    }
}

pub fn exec(name: &str, args: Value, ctx: &ToolCtx) -> ToolResult {
    tool(name).execute(&args, ctx).unwrap()
}
