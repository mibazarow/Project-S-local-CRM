use crate::agent::permissions::ConfirmDecision;
use crate::error::{AppError, Result};
use notify::RecommendedWatcher;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct AppState {
    pub working_dir: Mutex<Option<PathBuf>>,
    pub db: Mutex<Option<Connection>>,
    pub watcher: Mutex<Option<RecommendedWatcher>>,
    // Live MCP servers (SPEC 6.2): started with the working dir, restarted
    // by mcp_reload, shut down on app exit.
    pub mcp: Mutex<crate::mcp::McpRegistry>,
    // Run/tool journal (SPEC 3.3): opened with the working dir, shared by
    // every run thread through the app state.
    pub journal: Mutex<Option<crate::journal::Journal>>,
    // Emitter seam for background emitters that only hold AppState (the
    // rename_chat and todowrite tools): production installs the Tauri app
    // handle once in setup and clones it into every run thread's state, so
    // tools can emit UI events like "sessions-changed"/"todos-changed";
    // tests install a recording sink to assert those events.
    pub emitter: Mutex<Option<Arc<dyn UiEmitter>>>,
}

// Minimal UI event sink behind AppState.emitter (see field comment).
pub trait UiEmitter: Send + Sync {
    fn emit_event(&self, event: &str, payload: serde_json::Value);
}

impl UiEmitter for tauri::AppHandle {
    fn emit_event(&self, event: &str, payload: serde_json::Value) {
        use tauri::Emitter;
        let _ = self.emit(event, payload);
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn working_dir(&self) -> crate::error::Result<PathBuf> {
        self.working_dir
            .lock()
            .unwrap()
            .clone()
            .ok_or(crate::error::AppError::NotConfigured)
    }
}

// Identifies which run a confirmation or cancel request belongs to: a project
// chat or a harness session. Project and session ids come from different tables
// and can collide numerically, so the registry is keyed by this, not a bare i64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunKey {
    Project(i64),
    Session(i64),
    // The singleton notes chat: no project or session id, the key itself
    // identifies the run (SPEC notes chat slice).
    Notes,
}

impl RunKey {
    pub fn project_id(self) -> Option<i64> {
        match self {
            RunKey::Project(id) => Some(id),
            RunKey::Session(_) | RunKey::Notes => None,
        }
    }

    pub fn session_id(self) -> Option<i64> {
        match self {
            RunKey::Session(id) => Some(id),
            RunKey::Project(_) | RunKey::Notes => None,
        }
    }

    pub fn is_notes(self) -> bool {
        matches!(self, RunKey::Notes)
    }
}

// In-memory bridge between the blocking agent thread and the async UI
// (SPEC 6 "подтверждения инструментов — единый механизм"): pending
// confirmations wait on a channel until resolve_confirmation sends the
// decision; allow-always tool names are remembered for the session; cancel
// flags let stop requests interrupt a run between loop steps.
#[derive(Default)]
pub struct AgentRegistry {
    pending: Mutex<HashMap<u64, (String, Sender<ConfirmDecision>)>>,
    allow_always: Mutex<HashSet<String>>,
    cancels: Mutex<HashMap<RunKey, Arc<AtomicBool>>>,
    next_id: AtomicU64,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // Registers a pending confirmation and returns its id plus the receiver
    // the agent thread blocks on.
    pub fn register(&self, tool: &str) -> (u64, Receiver<ConfirmDecision>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, (tool.to_string(), tx));
        (id, rx)
    }

    pub fn resolve(&self, id: u64, decision: ConfirmDecision) -> Result<()> {
        let (tool, tx) = self
            .pending
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| AppError::Invalid(format!("unknown confirmation id: {id}")))?;
        if decision == ConfirmDecision::AllowAlways {
            self.allow_always.lock().unwrap().insert(tool);
        }
        tx.send(decision)
            .map_err(|_| AppError::Invalid(format!("confirmation {id} is no longer waiting")))
    }

    pub fn is_allowed(&self, tool: &str) -> bool {
        self.allow_always.lock().unwrap().contains(tool)
    }

    // Drops a pending entry after its receiver timed out.
    pub fn forget(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    pub fn set_cancel(&self, run: RunKey, flag: &Arc<AtomicBool>) {
        self.cancels
            .lock()
            .unwrap()
            .insert(run, Arc::clone(flag));
    }

    pub fn clear_cancel(&self, run: RunKey) {
        self.cancels.lock().unwrap().remove(&run);
    }

    // Returns false when no run is active for the key.
    pub fn request_cancel(&self, run: RunKey) -> bool {
        match self.cancels.lock().unwrap().get(&run) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}
