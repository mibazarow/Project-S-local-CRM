// Search reindex manager («Переиндексация поиска»): one pass over every
// searchable entity — each project (incremental index_root), the Notes vault,
// the tasks_fts mirror, harness chat message vectors and the memory
// mirror/vectors. Manual start from the top bar plus an automatic pass every
// 5 minutes driven by a 30 s background tick; the pass runs on its own thread
// with its own DB connection and never blocks the UI. Status is global
// (Mutex-protected) so the button can poll it; progress arrives as
// "reindex-progress" events and the finish as "reindex-done".
use crate::chat;
use crate::sync_util::lock_recover;
use crate::db;
use crate::embed;
use crate::error::Result;
use crate::index;
use crate::memory;
use crate::paths;
use crate::projects;
use crate::state::{AppState, UiEmitter};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

// Countdown between passes, also the reset value after every completion.
pub const REINDEX_INTERVAL_MS: i64 = 5 * 60 * 1000;

// Auto-tick period: the countdown is checked this often (same cadence as the
// scheduler thread).
pub const TICK_SECONDS: u64 = 30;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReindexStatus {
    pub running: bool,
    pub done: i64,
    pub total: i64,
    // Human-readable label of the unit being indexed (Russian, for the UI).
    pub phase: String,
    // Epoch milliseconds; 0 until the first countdown is armed.
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
}

#[derive(Default)]
pub struct ReindexManager {
    status: Mutex<ReindexStatus>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ReindexManager {
    pub fn status(&self) -> ReindexStatus {
        lock_recover(&self.status).clone()
    }

    // First countdown after start: now + 5 min, so the app never reindexes
    // immediately on launch.
    pub fn arm_initial(&self) {
        let mut st = lock_recover(&self.status);
        if st.next_run_at == 0 {
            st.next_run_at = now_ms() + REINDEX_INTERVAL_MS;
        }
    }

    // Manual start: spawns the background pass unless one is already running;
    // always returns the current status.
    pub fn start(self: &Arc<Self>, wd: PathBuf, emitter: Arc<dyn UiEmitter>) -> ReindexStatus {
        if !self.begin() {
            return self.status();
        }
        let mgr = Arc::clone(self);
        std::thread::spawn(move || {
            if let Err(e) = mgr.run_reindex(&wd, emitter.as_ref()) {
                eprintln!("reindex failed: {e}");
            }
        });
        self.status()
    }

    // Automatic start: fires only when the countdown elapsed and nothing runs.
    pub fn start_if_due(self: &Arc<Self>, wd: PathBuf, emitter: Arc<dyn UiEmitter>) {
        let st = self.status();
        if st.running || now_ms() < st.next_run_at {
            return;
        }
        self.start(wd, emitter);
    }

    // One full pass with the configured embedder.
    pub fn run_reindex(&self, wd: &Path, emitter: &dyn UiEmitter) -> Result<()> {
        let embedder = embed::select_embedder(wd);
        self.run_reindex_with(wd, emitter, embedder.as_ref())
    }

    // Core pass with an injected embedder (tests pass a fake). Finalizes the
    // status and emits "reindex-done" even when a unit failed, so the UI
    // countdown always resets.
    pub fn run_reindex_with(
        &self,
        wd: &Path,
        emitter: &dyn UiEmitter,
        embedder: &dyn embed::Embedder,
    ) -> Result<()> {
        let result = self.reindex_units(wd, emitter, embedder);
        let (done, total) = {
            let mut st = lock_recover(&self.status);
            st.running = false;
            st.last_run_at = Some(now_ms());
            st.next_run_at = now_ms() + REINDEX_INTERVAL_MS;
            (st.done, st.total)
        };
        emitter.emit_event("reindex-done", json!({ "done": done, "total": total }));
        result
    }

    // Units: one per project, then notes, tasks, chats, memory.
    fn reindex_units(
        &self,
        wd: &Path,
        emitter: &dyn UiEmitter,
        embedder: &dyn embed::Embedder,
    ) -> Result<()> {
        let conn = db::open(&paths::db_path(wd))?;
        let cfg = index::indexing_config(wd)?;
        let projects: Vec<(i64, String)> = {
            let mut stmt = conn.prepare("SELECT id, name FROM projects ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let total = (projects.len() + 4) as i64;
        lock_recover(&self.status).total = total;
        let mut done = 0i64;

        for (id, name) in &projects {
            let root = projects::project_root(&conn, wd, *id)?;
            index::index_root(&conn, *id, &root, &cfg, embedder)?;
            done += 1;
            self.progress(done, total, &format!("Проект: {name}"), emitter);
        }
        index::index_root(&conn, index::NOTES_SCOPE_ID, &paths::notes_dir(wd), &cfg, embedder)?;
        done += 1;
        self.progress(done, total, "Заметки", emitter);

        rebuild_tasks_fts(&conn)?;
        done += 1;
        self.progress(done, total, "Задачи", emitter);

        chat::index_chat_messages_with(&conn, embedder)?;
        done += 1;
        self.progress(done, total, "Чаты", emitter);

        memory::index_memory_with(&conn, embedder)?;
        done += 1;
        self.progress(done, total, "Память", emitter);
        Ok(())
    }

    fn progress(&self, done: i64, total: i64, phase: &str, emitter: &dyn UiEmitter) {
        {
            let mut st = lock_recover(&self.status);
            st.done = done;
            st.total = total;
            st.phase = phase.to_string();
        }
        emitter.emit_event(
            "reindex-progress",
            json!({ "done": done, "total": total, "phase": phase }),
        );
    }

    // Takes the single-run slot; false when a pass is already running.
    fn begin(&self) -> bool {
        let mut st = lock_recover(&self.status);
        if st.running {
            return false;
        }
        st.running = true;
        st.done = 0;
        st.total = 0;
        st.phase = String::new();
        true
    }
}

// tasks_fts is a contentless mirror normally synced per mutation by tasks.rs;
// a full delete + insert-select rebuild keeps it consistent with the tasks
// table at kanban scale (also heals rows left by legacy or direct-SQL edits).
// The mirror's legacy tags column is rebuilt empty: tags are out of the model.
fn rebuild_tasks_fts(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM tasks_fts", [])?;
    conn.execute(
        "INSERT INTO tasks_fts (rowid, title, comment, tags)
         SELECT id, title, comment, '' FROM tasks",
        [],
    )?;
    Ok(())
}

pub(crate) struct ReindexRuntime {
    manager: OnceLock<Arc<ReindexManager>>,
    timer_started: AtomicBool,
}

impl ReindexRuntime {
    const fn new() -> Self {
        ReindexRuntime {
            manager: OnceLock::new(),
            timer_started: AtomicBool::new(false),
        }
    }

    fn manager(&self) -> Arc<ReindexManager> {
        Arc::clone(self.manager.get_or_init(Default::default))
    }

    fn status(&self) -> ReindexStatus {
        self.manager().status()
    }

    fn ensure_timer_started<F>(&self, spawn: F) -> bool
    where
        F: FnOnce(),
    {
        if self.timer_started.swap(true, Ordering::SeqCst) {
            false
        } else {
            spawn();
            true
        }
    }
}

static RUNTIME: ReindexRuntime = ReindexRuntime::new();

pub fn global() -> Arc<ReindexManager> {
    RUNTIME.manager()
}

pub fn status() -> ReindexStatus {
    RUNTIME.status()
}

// Background auto-reindex, started once per process (setup + choose_working_dir
// like scheduler::start): arms the first 5-minute countdown, then every 30 s
// fires a pass when the countdown elapsed and nothing is running. The thread
// re-reads the working dir each tick, so switching it needs no restart.
pub fn start_timer(app: AppHandle) {
    let mgr = global();
    mgr.arm_initial();
    RUNTIME.ensure_timer_started(move || spawn_timer_thread(app, mgr));
}

fn spawn_timer_thread(app: AppHandle, mgr: Arc<ReindexManager>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(TICK_SECONDS));
            let wd = lock_recover(&app.state::<AppState>().working_dir).clone();
            if let Some(wd) = wd {
                mgr.start_if_due(wd, Arc::new(app.clone()));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness;
    use crate::llm::Role;
    use crate::onboarding::temp_test_dir;
    use crate::tasks;
    use rusqlite::params;
    use std::fs;
    use std::path::PathBuf;

    struct RecordingEmitter {
        events: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl RecordingEmitter {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }
        fn events(&self) -> Vec<(String, serde_json::Value)> {
            self.events.lock().unwrap().clone()
        }
    }

    impl UiEmitter for RecordingEmitter {
        fn emit_event(&self, event: &str, payload: serde_json::Value) {
            self.events.lock().unwrap().push((event.to_string(), payload));
        }
    }

    // Deterministic stand-in: byte-bucket counts, no network.
    struct FakeEmbedder {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeEmbedder {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl embed::Embedder for FakeEmbedder {
        fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 768];
                    for b in t.bytes() {
                        v[b as usize % 768] += 1.0;
                    }
                    v
                })
                .collect())
        }
        fn dim(&self) -> usize {
            embed::EMBED_DIM
        }
        fn backend_id(&self) -> &'static str {
            "fake"
        }
    }

    fn setup(tag: &str) -> (PathBuf, Connection) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        (wd, conn)
    }

    fn fts_count(conn: &Connection, table: &str, query: &str, extra: &str) -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE {table} MATCH ?1 {extra}"),
            params![query],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn wait_until_idle(mgr: &ReindexManager) {
        for _ in 0..100 {
            if !mgr.status().running {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("reindex pass did not finish in 5 s");
    }

    #[test]
    fn run_reindex_counts_units_and_indexes_every_entity() {
        let (wd, conn) = setup("reindex-all");
        let proj = projects::create_project(&conn, &wd, "proj").unwrap();
        fs::write(paths::projects_dir(&wd).join("proj/каталог.md"), "редизайн каталога").unwrap();
        fs::create_dir_all(paths::notes_dir(&wd)).unwrap();
        fs::write(paths::notes_dir(&wd).join("баннеры.md"), "заметка про баннер").unwrap();
        tasks::task_create(&conn, proj.id, "собрать макет логотипа", None).unwrap();
        // Legacy memory row written without the FTS mirror: the pass backfills it.
        conn.execute(
            "INSERT INTO memory (content, tags, created_at, updated_at)
             VALUES ('политика ценообразования', '[]', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let session = harness::create_session(&conn, Some("сессия")).unwrap();
        let chat_id = harness::session_chat_id(&conn, session.id).unwrap();
        chat::append_message(
            &conn,
            chat_id,
            Role::User,
            Some("обсуждение ценообразования"),
            &[],
            None,
            None,
        )
        .unwrap();

        let mgr = ReindexManager::default();
        let emitter = RecordingEmitter::new();
        mgr.run_reindex_with(&wd, &emitter, &FakeEmbedder::new()).unwrap();

        let st = mgr.status();
        assert_eq!((st.done, st.total), (5, 5)); // 1 project + notes + tasks + chats + memory
        assert!(!st.running);
        assert!(st.last_run_at.is_some());
        assert!(st.next_run_at > now_ms() + 4 * 60 * 1000);

        assert_eq!(
            fts_count(&conn, "files_fts", "редизайн", "AND project_id = '1'"),
            1
        );
        assert_eq!(
            fts_count(
                &conn,
                "files_fts",
                "баннер",
                &format!("AND project_id = '{}'", index::NOTES_SCOPE_ID)
            ),
            1
        );
        assert_eq!(
            fts_count(
                &conn,
                "tasks_fts",
                "логотипа",
                "AND tasks_fts.rowid IN (SELECT id FROM tasks WHERE project_id = 1)"
            ),
            1
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM message_vectors", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(fts_count(&conn, "memory_fts", "ценообразования", ""), 1);

        let events = emitter.events();
        let progress: Vec<_> = events
            .iter()
            .filter(|(e, _)| e == "reindex-progress")
            .collect();
        assert_eq!(progress.len(), 5);
        assert_eq!(progress[4].1["done"], 5);
        assert_eq!(progress[4].1["total"], 5);
        assert_eq!(progress[4].1["phase"], "Память");
        let done = events.iter().find(|(e, _)| e == "reindex-done").unwrap();
        assert_eq!(done.1["done"].as_i64(), Some(5));
        assert_eq!(done.1["total"].as_i64(), Some(5));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn second_pass_is_incremental_and_rebuilds_the_task_mirror() {
        let (wd, conn) = setup("reindex-inc");
        let proj = projects::create_project(&conn, &wd, "proj").unwrap();
        let root = paths::projects_dir(&wd).join("proj");
        fs::write(root.join("a.md"), "первое слово").unwrap();
        fs::write(root.join("b.md"), "второе слово").unwrap();

        let mgr = ReindexManager::default();
        let emitter = RecordingEmitter::new();
        let fake = FakeEmbedder::new();
        mgr.run_reindex_with(&wd, &emitter, &fake).unwrap();
        let calls_after_first = fake.calls();
        assert!(calls_after_first > 0);

        fs::remove_file(root.join("b.md")).unwrap();
        fs::write(root.join("c.md"), "третье слово").unwrap();
        // Task title changed behind tasks.rs's back (direct SQL): the mirror
        // lags until the rebuild.
        let t = tasks::task_create(&conn, proj.id, "старое слово", None).unwrap();
        conn.execute("UPDATE tasks SET title = 'новое слово' WHERE id = ?1", params![t.id])
            .unwrap();

        mgr.run_reindex_with(&wd, &emitter, &fake).unwrap();

        // Unchanged files re-embedded nothing: only the new c.md cost calls.
        assert_eq!(fake.calls(), calls_after_first + 1);
        assert_eq!(fts_count(&conn, "files_fts", "первое", ""), 1);
        assert!(fts_count(&conn, "files_fts", "второе", "") == 0); // deleted file pruned
        assert_eq!(fts_count(&conn, "files_fts", "третье", ""), 1); // new file added
        assert_eq!(fts_count(&conn, "tasks_fts", "новое", ""), 1); // mirror rebuilt
        assert_eq!(fts_count(&conn, "tasks_fts", "старое", ""), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn manual_start_sets_running_and_a_second_click_starts_nothing() {
        let (wd, conn) = setup("reindex-conc");
        projects::create_project(&conn, &wd, "proj").unwrap();

        let mgr = Arc::new(ReindexManager::default());
        let emitter = Arc::new(RecordingEmitter::new());
        let sink: Arc<dyn UiEmitter> = emitter.clone();
        mgr.start(wd.clone(), Arc::clone(&sink));
        wait_until_idle(&mgr);
        let st = mgr.status();
        assert!(!st.running);
        assert_eq!((st.done, st.total), (5, 5));
        assert!(st.next_run_at > now_ms() + 4 * 60 * 1000); // countdown reset after completion

        // Simulated concurrent pass: a second start is a no-op, no new thread.
        mgr.status.lock().unwrap().running = true;
        let second = mgr.start(wd.clone(), Arc::clone(&sink));
        assert!(second.running);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            emitter
                .events()
                .iter()
                .filter(|(e, _)| e == "reindex-done")
                .count(),
            1 // still exactly one finished pass
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_tick_fires_only_after_the_countdown_elapsed() {
        let (wd, conn) = setup("reindex-auto");
        projects::create_project(&conn, &wd, "proj").unwrap();

        let mgr = Arc::new(ReindexManager::default());
        let emitter = Arc::new(RecordingEmitter::new());
        let sink: Arc<dyn UiEmitter> = emitter.clone();
        mgr.arm_initial();
        let armed = mgr.status().next_run_at;
        assert!(armed > now_ms() + 4 * 60 * 1000 && armed <= now_ms() + 5 * 60 * 1000);

        // Not due yet: the tick must not start a pass.
        mgr.start_if_due(wd.clone(), Arc::clone(&sink));
        assert!(!mgr.status().running);

        // Due: the tick starts one pass and the countdown resets afterwards.
        mgr.status.lock().unwrap().next_run_at = now_ms() - 1;
        mgr.start_if_due(wd.clone(), Arc::clone(&sink));
        wait_until_idle(&mgr);
        let st = mgr.status();
        assert_eq!((st.done, st.total), (5, 5));
        assert!(st.last_run_at.is_some());
        assert!(st.next_run_at > now_ms() + 4 * 60 * 1000);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn runtime_manager_is_created_once_and_shared() {
        let rt = ReindexRuntime::new();
        assert!(Arc::ptr_eq(&rt.manager(), &rt.manager()));
        assert!(Arc::ptr_eq(&global(), &global()));
    }

    #[test]
    fn runtime_status_reads_the_shared_manager() {
        let rt = ReindexRuntime::new();
        assert_eq!(rt.status().next_run_at, 0);
        rt.manager().arm_initial();
        let armed = rt.status().next_run_at;
        assert!(armed > now_ms() + 4 * 60 * 1000 && armed <= now_ms() + 5 * 60 * 1000);
    }

    #[test]
    fn ensure_timer_started_spawns_the_thread_exactly_once() {
        use std::sync::atomic::AtomicUsize;
        let rt = ReindexRuntime::new();
        let spawns = Arc::new(AtomicUsize::new(0));

        let first = {
            let spawns = spawns.clone();
            rt.ensure_timer_started(move || {
                spawns.fetch_add(1, Ordering::SeqCst);
            })
        };
        assert!(first, "first ensure must spawn the thread");
        assert_eq!(spawns.load(Ordering::SeqCst), 1);

        let second = {
            let spawns = spawns.clone();
            rt.ensure_timer_started(move || {
                spawns.fetch_add(1, Ordering::SeqCst);
            })
        };
        assert!(!second, "second ensure must not spawn");
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ensure_timer_started_spawns_once_under_concurrent_callers() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Barrier;
        let rt = Arc::new(ReindexRuntime::new());
        let spawns = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let rt = rt.clone();
            let spawns = spawns.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                rt.ensure_timer_started(move || {
                    spawns.fetch_add(1, Ordering::SeqCst);
                })
            }));
        }
        let spawned: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|did_spawn| *did_spawn)
            .collect();
        assert_eq!(spawned.len(), 1, "exactly one caller must have spawned");
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ensure_timer_started_does_not_retry_after_a_failed_spawn() {
        use std::panic::AssertUnwindSafe;
        use std::sync::atomic::AtomicUsize;
        let rt = ReindexRuntime::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_in = attempts.clone();
        let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            rt.ensure_timer_started(move || {
                attempts_in.fetch_add(1, Ordering::SeqCst);
                panic!("spawn failed");
            })
        }))
        .is_err();
        assert!(panicked, "the injected spawn must fail");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            !rt.ensure_timer_started(|| panic!("must not spawn again")),
            "a failed spawn must not be retried"
        );
    }
}
