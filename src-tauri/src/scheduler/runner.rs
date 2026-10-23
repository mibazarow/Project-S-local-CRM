// The fire path (SPEC 5.3): the session a due schedule runs in, the per-kind
// fire, the storm guard and the single retry of a failed run, driven by one
// tick pass over the due schedules.
use crate::error::{AppError, Result};
use crate::harness;
use crate::scheduler::notifier::Notifier;
use crate::scheduler::schedule::{fmt_utc, next_cron_after, resolve_timezone};
use crate::scheduler::store::{due_ids, mark_done, mark_error, schedule_dto, ScheduleDto};
use crate::state::{AgentRegistry, AppState};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

// Storm guard: a schedule whose last run started less than this long ago is
// never fired again, even when next_run_at says due (a bad spec or a manual
// next_run_at edit could otherwise fire every 30 s tick). Also the delay
// before the single retry of a failed run.
pub const MIN_RUN_INTERVAL_SECONDS: i64 = 60;

// A failed run is retried exactly once (retry_count < MAX_RETRIES); the
// second failure flips the schedule to error.
pub const MAX_RETRIES: i64 = 1;

// Starting a `run_at`/`cron` schedule: a harness run in the schedule's
// session with `prompt` as the user turn, through the shared run bridge.
// When the schedule has no session (never had one, or it was deleted — v12
// keeps schedules and nulls their session_id) the engine first creates a
// fresh session and re-points the schedule at it. The tick thread already
// holds the app DB mutex, so the runner receives the tick's connection
// instead of locking it again. Managed state is resolved through the
// AppHandle so the background thread owns no borrowed handles. Tests inject
// a recording runner.
pub trait SessionRunner {
    fn run(&self, conn: &Connection, session_id: i64, prompt: &str) -> Result<()>;
}

pub struct TauriSessionRunner<'a> {
    app: &'a AppHandle,
}

impl<'a> TauriSessionRunner<'a> {
    pub fn new(app: &'a AppHandle) -> Self {
        TauriSessionRunner { app }
    }
}

impl SessionRunner for TauriSessionRunner<'_> {
    fn run(&self, conn: &Connection, session_id: i64, prompt: &str) -> Result<()> {
        let state = self.app.state::<AppState>();
        let registry = self.app.state::<Arc<AgentRegistry>>();
        let chat_id = harness::run_session_on(
            self.app,
            state.inner(),
            registry.inner(),
            conn,
            session_id,
            prompt,
            "schedule",
            &[],
        )?;
        let _ = self.app.emit(
            "schedule-fired",
            serde_json::json!({ "sessionId": session_id, "chatId": chat_id }),
        );
        Ok(())
    }
}

// The session a run_at/cron fire runs in: the schedule's own session while
// it exists; otherwise a fresh one (SPEC 5.3 decision — a schedule survives
// session deletion and creates a new session when it fires). The new id is
// stored back on the schedule so later fires reuse it, and it is the id the
// shared run bridge routes "agent-event"/"agent-confirmation" by.
fn ensure_run_session(conn: &Connection, s: &ScheduleDto) -> Result<i64> {
    if let Some(sid) = s.session_id {
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM sessions WHERE id = ?1",
            params![sid],
            |r| r.get(0),
        )?;
        if exists > 0 {
            return Ok(sid);
        }
    }
    let title = s.title.clone().unwrap_or_else(|| s.spec.clone());
    let session = harness::create_session(conn, Some(&title))?;
    conn.execute(
        "UPDATE schedules SET session_id = ?1 WHERE id = ?2",
        params![session.id, s.id],
    )?;
    Ok(session.id)
}

fn fire_due(
    conn: &Connection,
    id: i64,
    now: DateTime<Utc>,
    notifier: &dyn Notifier,
    runner: &dyn SessionRunner,
) -> Result<()> {
    let s = schedule_dto(conn, id)?;
    let tz = resolve_timezone(s.timezone.as_deref())?;
    match s.kind.as_str() {
        "notify" => {
            let body = s
                .prompt
                .clone()
                .filter(|p| !p.trim().is_empty())
                .unwrap_or_else(|| s.spec.clone());
            notifier.notify("Project-S", &body)?;
            mark_done(conn, id, now)
        }
        "run_at" => {
            let session_id = ensure_run_session(conn, &s)?;
            let prompt = s
                .prompt
                .clone()
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| AppError::Invalid("schedule prompt is empty".into()))?;
            runner.run(conn, session_id, &prompt)?;
            mark_done(conn, id, now)
        }
        // The next occurrence is computed BEFORE the run is spawned: a long
        // run must not shift the cron grid or double-fire the next point.
        "cron" => {
            let next = next_cron_after(&s.spec, tz, now)?;
            let session_id = ensure_run_session(conn, &s)?;
            let prompt = s
                .prompt
                .clone()
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| AppError::Invalid("schedule prompt is empty".into()))?;
            runner.run(conn, session_id, &prompt)?;
            conn.execute(
                "UPDATE schedules SET last_run_at = ?1, next_run_at = ?2, error = NULL, retry_count = 0 WHERE id = ?3",
                params![fmt_utc(now), fmt_utc(next), id],
            )?;
            Ok(())
        }
        other => Err(AppError::Invalid(format!("unknown schedule kind: {other}"))),
    }
}

// First failure of a run: keep the schedule active, arm a single retry at
// now + MIN_RUN_INTERVAL_SECONDS and bump retry_count. Returns false when the
// retry budget is spent (retry_count >= MAX_RETRIES) and the caller must flip
// the schedule to error instead.
fn schedule_retry(conn: &Connection, id: i64, now: DateTime<Utc>) -> Result<bool> {
    let retry_count: i64 = conn.query_row(
        "SELECT retry_count FROM schedules WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    if retry_count >= MAX_RETRIES {
        return Ok(false);
    }
    conn.execute(
        "UPDATE schedules SET status = 'active', next_run_at = ?1, error = NULL, retry_count = ?2 WHERE id = ?3",
        params![
            fmt_utc(now + chrono::Duration::seconds(MIN_RUN_INTERVAL_SECONDS)),
            retry_count + 1,
            id,
        ],
    )?;
    Ok(true)
}

fn execute_due(
    conn: &Connection,
    id: i64,
    now: DateTime<Utc>,
    notifier: &dyn Notifier,
    runner: &dyn SessionRunner,
) -> Result<bool> {
    // A failed run gets exactly one retry after MIN_RUN_INTERVAL_SECONDS
    // (SPEC 5.3 lifecycle); the second failure flips the schedule to error
    // with the message, so the pass never retries every tick.
    match fire_due(conn, id, now, notifier, runner) {
        Ok(()) => {}
        Err(e) => {
            if !schedule_retry(conn, id, now)? {
                mark_error(conn, id, &e.to_string())?;
            }
        }
    }
    Ok(true)
}

// One scheduler pass: fire every active schedule whose next_run_at is due,
// skipping any whose last run started within MIN_RUN_INTERVAL_SECONDS (storm
// guard). `now` is injected so tests never wait. A failing run is retried
// once after MIN_RUN_INTERVAL_SECONDS; the second failure flips it to
// status='error' with the message; the pass continues with the others.
// Returns whether anything fired (the caller emits "schedules-changed" and
// "sessions-changed": a fire touches the session's last_active_at and can
// create a fresh session when the original one was deleted).
pub fn tick(
    conn: &Connection,
    now: DateTime<Utc>,
    notifier: &dyn Notifier,
    runner: &dyn SessionRunner,
) -> Result<bool> {
    let mut fired = false;
    for id in due_ids(conn, now)? {
        fired |= execute_due(conn, id, now, notifier, runner)?;
    }
    Ok(fired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::store::{create, mark_run_completion, open_session, CreateIn};
    use crate::scheduler::testutil::{
        create_session_row, setup, utc, RecordingNotifier, RecordingRunner,
    };

    #[test]
    fn tick_fires_due_schedules_once_and_advances_them() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let notify = create(
            &conn,
            &CreateIn {
                kind: "notify",
                spec: "2026-10-16T10:00:00",
                prompt: Some("call the client"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            now,
        )
        .unwrap();
        let run_at = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:30:00",
                prompt: Some("do the thing"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            now,
        )
        .unwrap();
        let cron = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            now,
        )
        .unwrap();

        let notifier = RecordingNotifier::new();
        let runner = RecordingRunner::new();
        // The cron point (10:00 MSK = 07:00Z) is due on the 17th.
        let fired = tick(&conn, utc("2026-10-17T09:00:00Z"), &notifier, &runner).unwrap();
        assert!(fired);
        assert_eq!(
            *notifier.sent.lock().unwrap(),
            vec![("Project-S".to_string(), "call the client".to_string())]
        );
        // Q.md item 4: each schedule runs in its own dedicated chat.
        assert_eq!(
            *runner.runs.lock().unwrap(),
            vec![
                (run_at.session_id.unwrap(), "do the thing".to_string()),
                (cron.session_id.unwrap(), "daily".to_string()),
            ]
        );
        assert_ne!(run_at.session_id, cron.session_id);
        assert_ne!(notify.session_id, run_at.session_id);

        let s = schedule_dto(&conn, notify.id).unwrap();
        assert_eq!((s.status.as_str(), s.next_run_at.as_deref()), ("done", None));
        assert!(s.last_run_at.is_some());
        let s = schedule_dto(&conn, run_at.id).unwrap();
        assert_eq!((s.status.as_str(), s.next_run_at.as_deref()), ("done", None));
        let s = schedule_dto(&conn, cron.id).unwrap();
        assert_eq!((s.status.as_str(), s.next_run_at.as_deref()), ("active", Some("2026-10-18T07:00:00Z")));
        assert!(s.last_run_at.is_some());

        // Second tick at the same instant: nothing re-fires.
        let fired = tick(&conn, utc("2026-10-17T10:00:00Z"), &notifier, &runner).unwrap();
        assert!(!fired);
        assert_eq!(notifier.sent.lock().unwrap().len(), 1);
        assert_eq!(runner.runs.lock().unwrap().len(), 2);
    }

    #[test]
    fn startup_catch_up_fires_missed_runs_once_without_replaying_the_grid() {
        let conn = setup();
        let session = create_session_row(&conn);
        // A cron schedule last fired long ago: several grid points were missed
        // while the app was closed.
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, prompt, status, created_at, next_run_at, timezone)
             VALUES (?1, 'cron', '0 10 * * *', 'daily', 'active', '2026-10-01T00:00:00Z', '2026-10-12T07:00:00Z', 'Europe/Moscow')",
            params![session],
        )
        .unwrap();        // A one-shot whose instant passed while the app was closed.
        let session2 = create_session_row(&conn);
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, prompt, status, created_at, next_run_at, timezone)
             VALUES (?1, 'run_at', '2026-10-13T07:00:00Z', 'late one-shot', 'active', '2026-10-01T00:00:00Z', '2026-10-13T07:00:00Z', 'Europe/Moscow')",
            params![session2],
        )
        .unwrap();

        let runner = RecordingRunner::new();
        let fired = tick(
            &conn,
            utc("2026-10-16T08:00:00Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(fired);
        // Each missed schedule caught up exactly once; the cron row jumped to
        // the next FUTURE point, not one per missed day.
        assert_eq!(runner.runs.lock().unwrap().len(), 2);
        let next: Option<String> = conn
            .query_row("SELECT next_run_at FROM schedules WHERE kind = 'cron'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(next.as_deref(), Some("2026-10-17T07:00:00Z"));
        let done: i64 = conn
            .query_row("SELECT count(*) FROM schedules WHERE status = 'done'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(done, 1);
    }

    #[test]
    fn failing_runs_retry_once_and_the_second_failure_sets_status_error() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        // run_at whose run fails: the runner reports the failure.
        let run_at = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            now,
        )
        .unwrap();
        // notify whose delivery fails.
        let notify = create(
            &conn,
            &CreateIn {
                kind: "notify",
                spec: "2026-10-16T10:00:00",
                prompt: Some("ping"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            now,
        )
        .unwrap();

        // First failure: the retry budget arms one retry at now + 60 s.
        let fired = tick(
            &conn,
            utc("2026-10-16T11:00:00Z"),
            &RecordingNotifier::failing(),
            &RecordingRunner::failing(),
        )
        .unwrap();
        assert!(fired);
        for id in [run_at.id, notify.id] {
            let s = schedule_dto(&conn, id).unwrap();
            assert_eq!((s.status.as_str(), s.next_run_at.as_deref()), ("active", Some("2026-10-16T11:01:00Z")));
            let retry_count: i64 = conn
                .query_row("SELECT retry_count FROM schedules WHERE id = ?1", params![id], |r| r.get(0))
                .unwrap();
            assert_eq!(retry_count, 1);
        }

        // Second failure at the retry instant: status=error with the message.
        let fired = tick(
            &conn,
            utc("2026-10-16T11:01:00Z"),
            &RecordingNotifier::failing(),
            &RecordingRunner::failing(),
        )
        .unwrap();
        assert!(fired);
        let s = schedule_dto(&conn, run_at.id).unwrap();
        assert_eq!(s.status, "error");
        assert!(s.error.as_deref().unwrap().contains("session 999 not found"));
        let s = schedule_dto(&conn, notify.id).unwrap();
        assert_eq!(s.status, "error");
        assert!(s.error.as_deref().unwrap().contains("boom"));
    }

    #[test]
    fn rate_limit_skips_a_schedule_that_fired_within_the_last_minute() {
        let conn = setup();
        let cron = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();

        let runner = RecordingRunner::new();
        let fired = tick(
            &conn,
            utc("2026-10-17T09:00:00Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(fired);
        assert_eq!(runner.runs.lock().unwrap().len(), 1);

        // next_run_at says due, but the last run was 30 s ago: the tick must
        // not re-fire (a bad spec or manual edit cannot start a run storm).
        conn.execute(
            "UPDATE schedules SET next_run_at = '2026-10-17T09:00:00Z' WHERE id = ?1",
            params![cron.id],
        )
        .unwrap();
        let fired = tick(
            &conn,
            utc("2026-10-17T09:00:30Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(!fired, "rate-limited schedule fired inside the 60 s window");
        assert_eq!(runner.runs.lock().unwrap().len(), 1);

        // Past the window the same due schedule fires again (at exactly the
        // 60 s boundary the run is allowed).
        conn.execute(
            "UPDATE schedules SET next_run_at = '2026-10-17T09:01:00Z' WHERE id = ?1",
            params![cron.id],
        )
        .unwrap();
        let fired = tick(
            &conn,
            utc("2026-10-17T09:01:00Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(fired);
        assert_eq!(runner.runs.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_successful_run_resets_the_retry_counter() {
        let conn = setup();
        let cron = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        conn.execute(
            "UPDATE schedules SET retry_count = 1, next_run_at = '2026-10-17T09:00:00Z' WHERE id = ?1",
            params![cron.id],
        )
        .unwrap();

        let fired = tick(
            &conn,
            utc("2026-10-17T09:00:00Z"),
            &RecordingNotifier::new(),
            &RecordingRunner::new(),
        )
        .unwrap();
        assert!(fired);
        let retry_count: i64 = conn
            .query_row("SELECT retry_count FROM schedules WHERE id = ?1", params![cron.id], |r| r.get(0))
            .unwrap();
        assert_eq!(retry_count, 0);
        let s = schedule_dto(&conn, cron.id).unwrap();
        assert_eq!((s.status.as_str(), s.next_run_at.as_deref()), ("active", Some("2026-10-18T07:00:00Z")));
    }

    #[test]
    fn a_successful_one_shot_run_resets_the_retry_counter() {
        let conn = setup();
        let run_at = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        conn.execute(
            "UPDATE schedules SET retry_count = 1 WHERE id = ?1",
            params![run_at.id],
        )
        .unwrap();

        let fired = tick(
            &conn,
            utc("2026-10-16T11:00:00Z"),
            &RecordingNotifier::new(),
            &RecordingRunner::new(),
        )
        .unwrap();
        assert!(fired);
        let (status, retry_count): (String, i64) = conn
            .query_row(
                "SELECT status, retry_count FROM schedules WHERE id = ?1",
                params![run_at.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((status.as_str(), retry_count), ("done", 0));
    }

    #[test]
    fn firing_a_schedule_whose_session_was_detached_creates_a_fresh_one() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        // The TTL purge detaches a schedule (session_id SET NULL) without
        // deleting it; a later fire then creates a fresh session. (Deleting the
        // session through delete_session would have removed the schedule too.)
        conn.execute(
            "UPDATE schedules SET session_id = NULL WHERE id = ?1",
            params![s.id],
        )
        .unwrap();

        let runner = RecordingRunner::new();
        let fired = tick(
            &conn,
            utc("2026-10-16T11:00:00Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(fired);
        let after = schedule_dto(&conn, s.id).unwrap();
        let fresh = after
            .session_id
            .expect("the fire must re-point the schedule at a new session");
        // The runner receives exactly the session id the shared bridge then
        // routes every "agent-event"/"agent-confirmation" by.
        assert_eq!(
            *runner.runs.lock().unwrap(),
            vec![(fresh, "go".to_string())]
        );
        let title: Option<String> = conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![fresh], |r| r.get(0))
            .unwrap();
        // Q.md item 5: the recreated chat is titled after the schedule.
        assert_eq!(title.as_deref(), Some("go"));
        // The fresh session has its harness chat row: the run bridge needs it.
        assert!(harness::session_chat_id(&conn, fresh).is_ok());
    }

    #[test]
    fn firing_a_run_at_without_a_session_creates_one() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        conn.execute(
            "UPDATE schedules SET session_id = NULL WHERE id = ?1",
            params![s.id],
        )
        .unwrap();

        let runner = RecordingRunner::new();
        let fired = tick(
            &conn,
            utc("2026-10-16T11:00:00Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(fired);
        let after = schedule_dto(&conn, s.id).unwrap();
        let fresh = after.session_id.expect("a sessionless fire creates a session");
        assert_eq!(*runner.runs.lock().unwrap(), vec![(fresh, "go".to_string())]);
    }

    #[test]
    fn firing_reuses_the_existing_session_and_keeps_the_link() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        let own = s.session_id.expect("a new schedule owns its chat");

        let runner = RecordingRunner::new();
        let fired = tick(
            &conn,
            utc("2026-10-17T09:00:00Z"),
            &RecordingNotifier::new(),
            &runner,
        )
        .unwrap();
        assert!(fired);
        assert_eq!(
            *runner.runs.lock().unwrap(),
            vec![(own, "daily".to_string())]
        );
        let after = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(after.session_id, Some(own));
    }

    #[test]
    fn run_completion_marks_unread_yields_the_notice_and_clears_on_open() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-16T10:00:00", prompt: Some("go"), timezone: Some("Europe/Moscow"), related_task_id: None, title: Some("Утренняя сводка"), session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        let session = s.session_id.expect("a new schedule owns its chat");
        // Fire it so last_run_at is set (the completion lookup orders by it).
        tick(
            &conn,
            utc("2026-10-16T11:00:00Z"),
            &RecordingNotifier::new(),
            &RecordingRunner::new(),
        )
        .unwrap();

        let notice = mark_run_completion(
            &conn,
            session,
            "  Готово: цены сняты.  ",
            "finish",
            utc("2026-10-16T11:05:00Z"),
        )
        .unwrap()
        .expect("a fired run_at schedule must produce a completion notice");
        assert_eq!((notice.schedule_id, notice.session_id), (s.id, session));
        assert_eq!(notice.title, "Утренняя сводка");
        assert_eq!(notice.body, "Готово: цены сняты.");

        let dto = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(dto.unread_at.as_deref(), Some("2026-10-16T11:05:00Z"));

        // Opening the schedule's chat is the read mark: the stamp clears.
        assert_eq!(open_session(&conn, s.id).unwrap(), session);
        let dto = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(dto.unread_at, None);
    }

    #[test]
    fn run_completion_bodies_truncate_and_report_failed_finishes() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("daily report"), timezone: None, related_task_id: None, title: None, session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        let session = s.session_id.unwrap();

        // A long answer is cut to 120 chars plus the ellipsis.
        let long = "д".repeat(200);
        let notice = mark_run_completion(&conn, session, &long, "stop", utc("2026-10-16T11:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(notice.body.chars().count(), 121);
        assert!(notice.body.ends_with('…'));
        // No explicit title: it is derived from the prompt.
        assert_eq!(notice.title, "daily report");

        // A failed finish with no answer text reports the reason instead.
        let notice = mark_run_completion(&conn, session, "", "llm_error", utc("2026-10-16T12:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(notice.body, "Сбой выполнения: llm_error");

        // A failed finish whose answer carries the error text shows it.
        let notice = mark_run_completion(
            &conn,
            session,
            "Warning: transport failed",
            "llm_error",
            utc("2026-10-16T12:00:00Z"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(notice.body, "Warning: transport failed");
    }

    #[test]
    fn run_completion_skips_notify_schedules_and_sessions_without_one() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn { kind: "notify", spec: "2026-10-16T10:00:00", prompt: Some("ping"), timezone: None, related_task_id: None, title: None, session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        // notify schedules already notified at fire time: no completion
        // notice, no unread stamp.
        assert!(mark_run_completion(&conn, s.session_id.unwrap(), "answer", "finish", utc("2026-10-16T11:00:00Z"))
            .unwrap()
            .is_none());
        let dto = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(dto.unread_at, None);

        // A harness run in a session with no schedule: nothing to mark.
        assert!(mark_run_completion(&conn, 999, "answer", "finish", utc("2026-10-16T11:00:00Z"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn run_completion_targets_the_most_recently_fired_schedule_of_the_session() {
        let conn = setup();
        let session = create_session_row(&conn);
        let older = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-14T10:00:00Z", prompt: Some("a"), timezone: None, related_task_id: None, title: Some("older"), session_id: None },
            utc("2026-10-13T08:00:00Z"),
        )
        .unwrap();
        let newer = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("b"), timezone: None, related_task_id: None, title: Some("newer"), session_id: None },
            utc("2026-10-13T08:00:00Z"),
        )
        .unwrap();
        // Both schedules share one chat (the pre-dedicated-chat shape): the
        // completion lookup picks the most recently fired one of that session.
        conn.execute(
            "UPDATE schedules SET session_id = ?1 WHERE id IN (?2, ?3)",
            params![session, older.id, newer.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE schedules SET last_run_at = '2026-10-14T07:00:00Z' WHERE id = ?1",
            params![older.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE schedules SET last_run_at = '2026-10-16T07:00:00Z' WHERE id = ?1",
            params![newer.id],
        )
        .unwrap();

        let notice = mark_run_completion(&conn, session, "done", "finish", utc("2026-10-16T11:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!((notice.schedule_id, notice.title.as_str()), (newer.id, "newer"));
        let older = schedule_dto(&conn, older.id).unwrap();
        assert_eq!(older.unread_at, None);
        let newer = schedule_dto(&conn, newer.id).unwrap();
        assert_eq!(newer.unread_at.as_deref(), Some("2026-10-16T11:00:00Z"));
    }
}
