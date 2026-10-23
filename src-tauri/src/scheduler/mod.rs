// Scheduler engine (SPEC 5.3): one-shot `run_at`, recurring 5-field `cron`
// with an IANA timezone, and `notify` (native macOS notification, no model
// call). Schedules live in the `schedules` table; a background thread ticks
// every 30 s while the app runs. Missed runs (app was closed) are caught up
// by the first tick after start: each active schedule whose next_run_at is
// in the past fires exactly once, and a cron schedule then advances to the
// next FUTURE occurrence — missed points are never replayed as a burst
// (model of Ouroboros schedule_occurrence._advance, read-only reference).
// Lifecycle: active → paused (manual) / done (run_at fired) / error (a run
// failed; the message is kept in `error`). Timestamps are RFC3339 UTC so
// SQLite string comparison orders them.
//
// Seam for the LaunchAgent autostart slice (SPEC 5.3, separate slice): the
// catch-up rule above is exactly what a cold start needs — start() performs
// it via the immediate first tick, so the autostart slice only has to launch
// the app, not replay schedules.
//
// (R-06) The engine is split by concern into submodules: `store` (the
// `schedules` table: DTO, CRUD, session binding, unread marking), `schedule`
// (timezone, run_at/cron parsing, title derivation), `runner` (the fire path:
// session runner, due firing, retries, one tick pass), `notifier` (native
// notifications) and `service` (the background tick thread and `start`). The
// items the rest of the crate calls are re-exported here, so every external
// `scheduler::…` call path keeps working unchanged.
mod notifier;
mod runner;
mod schedule;
mod service;
mod store;

// Re-exported only for the external call sites that use them (commands,
// agent schedule tools, chat run path, lib.rs, db migrations); the rest of
// each submodule's API stays reachable through the submodules themselves.
pub use notifier::{Notifier, TauriNotifier, CLICK_ACTION_TYPE};
pub use service::{now_utc, start};
pub use store::{
    create, delete, list, mark_run_completion, open_session, pause, resume, CreateIn, ScheduleDto,
};
// The v23 title backfill in db::migrations derives titles with the same rule.
pub(crate) use schedule::derive_title;

// Test helpers shared by the submodule test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::error::{AppError, Result};
    use crate::harness;
    use crate::scheduler::notifier::Notifier;
    use crate::scheduler::runner::SessionRunner;
    use chrono::{DateTime, Utc};
    use chrono_tz::Tz;
    use rusqlite::{params, Connection};
    use std::str::FromStr;
    use std::sync::Mutex;

    pub(crate) struct RecordingNotifier {
        pub(crate) sent: Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    impl RecordingNotifier {
        pub(crate) fn new() -> Self {
            RecordingNotifier {
                sent: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        pub(crate) fn failing() -> Self {
            RecordingNotifier {
                sent: Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }

    impl Notifier for RecordingNotifier {
        fn notify(&self, title: &str, body: &str) -> Result<()> {
            if self.fail {
                return Err(AppError::Io("boom".into()));
            }
            self.sent.lock().unwrap().push((title.into(), body.into()));
            Ok(())
        }
    }

    pub(crate) struct RecordingRunner {
        pub(crate) runs: Mutex<Vec<(i64, String)>>,
        fail: bool,
    }

    impl RecordingRunner {
        pub(crate) fn new() -> Self {
            RecordingRunner {
                runs: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        pub(crate) fn failing() -> Self {
            RecordingRunner {
                runs: Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }

    impl SessionRunner for RecordingRunner {
        fn run(&self, _conn: &Connection, session_id: i64, prompt: &str) -> Result<()> {
            if self.fail {
                return Err(AppError::Invalid("session 999 not found".into()));
            }
            self.runs.lock().unwrap().push((session_id, prompt.into()));
            Ok(())
        }
    }

    pub(crate) fn setup() -> Connection {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn
    }

    pub(crate) fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    pub(crate) fn moscow() -> Tz {
        Tz::from_str("Europe/Moscow").unwrap()
    }

    pub(crate) fn create_session_row(conn: &Connection) -> i64 {
        harness::create_session(conn, Some("sched")).unwrap().id
    }

    // Appends one user turn to a session's harness chat, the way a real chat
    // run persists the message that triggered the agent.
    pub(crate) fn add_user_turn(conn: &Connection, session_id: i64, text: &str) {
        let chat_id = harness::session_chat_id(conn, session_id).unwrap();
        crate::chat::append_message(
            conn,
            chat_id,
            crate::llm::Role::User,
            Some(text),
            &[],
            None,
            None,
        )
        .unwrap();
    }

    pub(crate) fn session_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    pub(crate) fn session_title(conn: &Connection, session_id: i64) -> Option<String> {
        conn.query_row(
            "SELECT title FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap()
    }
}
