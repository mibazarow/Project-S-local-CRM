use crate::config::ConfigKind;
use crate::sync_util::lock_recover;
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
    *lock_recover(&state.watcher) = Some(watcher);

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
    let guard = lock_recover(&state.db);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use notify::EventKind;
    use std::fs;

    // The debounce/coalesce loop itself lives inside `start`'s thread and
    // needs a live tauri::AppHandle plus real FS events, so it is not
    // unit-testable without changing production code. What IS deterministic
    // and pinned here: the pure per-event classification helpers that the
    // debounced loop aggregates — those decide WHICH reindexes and config
    // notifications a burst produces once collapsed.

    fn event(paths: &[&str]) -> Event {
        Event {
            kind: EventKind::Any,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        }
    }

    #[test]
    fn touched_matches_only_paths_under_root() {
        let root = Path::new("/wd/Notes");
        let ev = event(&["/wd/Notes/sub/a.md", "/wd/Projects/other/b.md"]);
        assert!(touched(root, &ev));
        let outside = event(&["/wd/Projects/other/b.md"]);
        assert!(!touched(root, &outside));
        // A sibling sharing the root's name prefix is not "under" it.
        assert!(!touched(root, &event(&["/wd/NotesX/a.md"])));
    }

    #[test]
    fn changed_kinds_maps_config_filenames_and_dedupes() {
        let ev = event(&[
            "/wd/Settings/config/llm.json",
            "/wd/Settings/config/app.json",
            "/wd/elsewhere/llm.json",
            "/wd/random.txt",
            "/wd/indexing.json",
        ]);
        let kinds = changed_kinds(&ev);
        assert_eq!(kinds.len(), 3, "{kinds:?}");
        assert!(kinds.contains(&ConfigKind::Llm));
        assert!(kinds.contains(&ConfigKind::App));
        assert!(kinds.contains(&ConfigKind::Indexing));
        assert!(changed_kinds(&event(&["notes.md", "project-s.db"])).is_empty());
    }

    #[test]
    fn dirty_projects_collapses_burst_per_project_and_ignores_outside() {
        let wd = temp_test_dir("watch-dirty");
        let state = AppState::new();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let one = projects::create_project(&conn, &wd, "one").unwrap();
        let two = projects::create_project(&conn, &wd, "two").unwrap();
        *state.db.lock().unwrap() = Some(conn);

        let root_one = paths::projects_dir(&wd).join("one");
        let ev = event(&[
            &root_one.join("a.md").display().to_string(),
            &root_one.join("sub/deep/b.md").display().to_string(),
            &paths::projects_dir(&wd).join("two/c.md").display().to_string(),
        ]);
        let ids = dirty_projects(&state, &ev);
        assert_eq!(ids, HashSet::from([one.id, two.id]));

        // Events outside every project folder dirty nothing.
        assert!(dirty_projects(&state, &event(&["/somewhere/else.md"])).is_empty());
        // No DB installed: nothing to match against.
        assert!(dirty_projects(&AppState::new(), &ev).is_empty());

        let listed = {
            let guard = state.db.lock().unwrap();
            project_paths(guard.as_ref().unwrap())
        };
        assert_eq!(listed.len(), 2);
        assert!(listed.contains(&root_one));
        fs::remove_dir_all(&wd).unwrap();
    }
}
