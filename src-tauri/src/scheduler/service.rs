// The facade entry points and the background tick thread (SPEC 5.3): `start`
// runs once per process, the immediate first tick performs the startup
// catch-up, then a tick every TICK_SECONDS while the app runs.
use crate::scheduler::notifier::TauriNotifier;
use crate::scheduler::runner::{tick, TauriSessionRunner};
use crate::state::AppState;
use crate::sync_util::lock_recover;
use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter, Manager};

pub const TICK_SECONDS: u64 = 30;

pub(crate) struct SchedulerRuntime {
    started: AtomicBool,
}

impl SchedulerRuntime {
    const fn new() -> Self {
        SchedulerRuntime {
            started: AtomicBool::new(false),
        }
    }

    fn ensure_started<F>(&self, spawn: F) -> bool
    where
        F: FnOnce(),
    {
        if self.started.swap(true, Ordering::SeqCst) {
            false
        } else {
            spawn();
            true
        }
    }
}

static RUNTIME: SchedulerRuntime = SchedulerRuntime::new();

// Starts the background tick thread once per process: an immediate first
// tick performs the startup catch-up (SPEC 5.3), then a tick every 30 s.
// The thread waits for the working dir to be configured and stops with the
// process. LaunchAgent autostart (separate slice) only needs to launch the
// app — this first tick is the catch-up seam.
pub fn start(app: AppHandle) {
    RUNTIME.ensure_started(move || spawn_tick_thread(app));
}

fn spawn_tick_thread(app: AppHandle) {
    std::thread::spawn(move || {
        loop {
            let state = app.state::<AppState>();
            if lock_recover(&state.working_dir).is_some() {
                let notifier = TauriNotifier::new(&app);
                let runner = TauriSessionRunner::new(&app);
                let guard = lock_recover(&state.db);
                if let Some(conn) = guard.as_ref() {
                    let now = Utc::now();
                    match tick(conn, now, &notifier, &runner) {
                        Ok(true) => {
                            let _ = app.emit("schedules-changed", serde_json::json!({}));
                            // The fired run touched its session (and a fire
                            // may have created a fresh one): the UI refreshes
                            // the session list so the new session appears.
                            let _ = app.emit("sessions-changed", serde_json::json!({}));
                        }
                        Ok(false) => {}
                        Err(e) => eprintln!("scheduler tick failed: {e}"),
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(TICK_SECONDS));
        }
    });
}

// Wall-clock now in the fixed UTC storage format.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn ensure_started_spawns_the_thread_exactly_once() {
        use std::sync::atomic::AtomicUsize;
        let runtime = SchedulerRuntime::new();
        let spawns = Arc::new(AtomicUsize::new(0));

        let first = {
            let spawns = spawns.clone();
            runtime.ensure_started(move || {
                spawns.fetch_add(1, Ordering::SeqCst);
            })
        };
        assert!(first, "first ensure must spawn the thread");
        assert_eq!(spawns.load(Ordering::SeqCst), 1);

        let second = {
            let spawns = spawns.clone();
            runtime.ensure_started(move || {
                spawns.fetch_add(1, Ordering::SeqCst);
            })
        };
        assert!(!second, "second ensure must not spawn");
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ensure_started_spawns_once_under_concurrent_callers() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Barrier;
        let runtime = Arc::new(SchedulerRuntime::new());
        let spawns = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let runtime = runtime.clone();
            let spawns = spawns.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                runtime.ensure_started(move || {
                    spawns.fetch_add(1, Ordering::SeqCst);
                })
            }));
        }
        let started: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "exactly one thread spawned");
        assert_eq!(
            started.iter().filter(|s| **s).count(),
            1,
            "exactly one caller reports started"
        );
    }

    #[test]
    fn ensure_started_does_not_retry_after_a_failed_spawn() {
        use std::panic::AssertUnwindSafe;
        use std::sync::atomic::AtomicUsize;
        let runtime = SchedulerRuntime::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_in = attempts.clone();
        let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            runtime.ensure_started(move || {
                attempts_in.fetch_add(1, Ordering::SeqCst);
                panic!("spawn failed");
            })
        }))
        .is_err();
        assert!(panicked, "the injected spawn must fail");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            !runtime.ensure_started(|| panic!("must not spawn again")),
            "a failed spawn must not be retried"
        );
    }
}
