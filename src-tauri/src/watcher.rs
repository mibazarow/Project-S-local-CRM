use crate::config::ConfigKind;
use crate::index;
use crate::paths;
use crate::state::AppState;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tauri::{Emitter, Manager};

const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigChanged {
    which: &'static str,
}

// (Re)starts the watcher on Settings/config/, Settings/skills/, the Notes/
// vault and every project folder for a working dir. Restarting re-attaches
// watches after project create/rename. Skills are re-read from disk on every
// use, so the watcher only needs to notify (hand edits land without a
// restart).
pub fn start(app: &tauri::AppHandle, state: &AppState, wd: &Path) {
    let (tx, rx) = mpsc::channel();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut watching = false;
    let config_dir = paths::config_dir(wd);
    if config_dir.is_dir() && watcher.watch(&config_dir, RecursiveMode::Recursive).is_ok() {
        watching = true;
    }
    let skills_root = paths::skills_dir(wd);
    if skills_root.is_dir() && watcher.watch(&skills_root, RecursiveMode::Recursive).is_ok() {
        watching = true;
    }
    let notes_root = paths::notes_dir(wd);
    if notes_root.is_dir() && watcher.watch(&notes_root, RecursiveMode::Recursive).is_ok() {
        watching = true;
    }
    if let Ok(conn) = crate::db::open(&paths::db_path(wd)) {
        for path in project_paths(&conn) {
            if path.is_dir() && watcher.watch(&path, RecursiveMode::Recursive).is_ok() {
                watching = true;
            }
        }
    }
    if !watching {
        return;
    }
    // Replacing the watcher drops the old one, disconnecting its channel
    // and ending the old debounce thread.
    *state.watcher.lock().unwrap() = Some(watcher);

    let app = app.clone();
    thread::spawn(move || {
        let state = app.state::<AppState>();
        while let Ok(Ok(event)) = rx.recv() {
            let mut changed = changed_kinds(&event);
            let mut dirty = dirty_projects(&state, &event);
            let mut skills_dirty = touched(&skills_root, &event);
            let mut notes_dirty = touched(&notes_root, &event);
            while let Ok(Ok(event)) = rx.recv_timeout(DEBOUNCE) {
                changed.extend(changed_kinds(&event));
                dirty.extend(dirty_projects(&state, &event));
                skills_dirty |= touched(&skills_root, &event);
                notes_dirty |= touched(&notes_root, &event);
            }
            for kind in changed {
                let _ = app.emit("config-changed", ConfigChanged { which: kind.which() });
            }
            if skills_dirty {
                let _ = app.emit("skills-changed", serde_json::json!({}));
            }
            if notes_dirty {
                if let Ok(wd) = state.working_dir() {
                    if let Ok(stats) = index::index_notes(&wd) {
                        let _ = app.emit(
                            "index-changed",
                            index::IndexChanged {
                                project_id: index::NOTES_SCOPE_ID,
                                stats,
                            },
                        );
                    }
                }
            }
            for project_id in dirty {
                if let Ok(stats) = index::index_project(&state, project_id) {
                    let _ = app.emit(
                        "index-changed",
                        index::IndexChanged { project_id, stats },
                    );
                    // The watcher already debounced the burst of file
                    // events; the brief itself only regenerates when stale.
                    crate::brief::schedule_refresh(&app, project_id);
                }
            }
        }
    });
}

// True when any event path lies under `root`.
fn touched(root: &Path, event: &Event) -> bool {
    event.paths.iter().any(|p| p.starts_with(root))
}

fn changed_kinds(event: &Event) -> HashSet<ConfigKind> {
    event
        .paths
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| ConfigKind::from_filename(n.to_str()?)))
        .collect()
}

// Projects whose folder contains one of the event paths; events for one
// project collapse into a single reindex.
fn dirty_projects(state: &AppState, event: &Event) -> HashSet<i64> {
    let guard = state.db.lock().unwrap();
    let Some(conn) = guard.as_ref() else {
        return HashSet::new();
    };
    let ids: Vec<(i64, PathBuf)> = match conn.prepare("SELECT id, path FROM projects") {
        Ok(mut stmt) => stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map(|rows| {
                rows.flatten()
                    .map(|(id, path)| (id, PathBuf::from(path)))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    ids.into_iter()
        .filter(|(_, root)| event.paths.iter().any(|p| p.starts_with(root)))
        .map(|(id, _)| id)
        .collect()
}

fn project_paths(conn: &Connection) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare("SELECT path FROM projects") else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
        for row in rows.flatten() {
            out.push(PathBuf::from(row));
        }
    }
    out
}
