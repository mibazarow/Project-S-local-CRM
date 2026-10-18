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
use crate::error::{AppError, Result};
use crate::harness;
use crate::state::{AgentRegistry, AppState};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

pub const TICK_SECONDS: u64 = 30;

// Storm guard: a schedule whose last run started less than this long ago is
// never fired again, even when next_run_at says due (a bad spec or a manual
// next_run_at edit could otherwise fire every 30 s tick). Also the delay
// before the single retry of a failed run.
pub const MIN_RUN_INTERVAL_SECONDS: i64 = 60;

// A failed run is retried exactly once (retry_count < MAX_RETRIES); the
// second failure flips the schedule to error.
pub const MAX_RETRIES: i64 = 1;

pub const KINDS: [&str; 3] = ["run_at", "cron", "notify"];

const COLS: &str = "id, session_id, kind, spec, prompt, status, related_task_id, created_at, last_run_at, next_run_at, timezone, error, title";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleDto {
    pub id: i64,
    pub session_id: Option<i64>,
    pub kind: String,
    pub spec: String,
    pub prompt: Option<String>,
    pub status: String,
    pub related_task_id: Option<i64>,
    pub created_at: String,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub timezone: Option<String>,
    pub error: Option<String>,
    pub title: Option<String>,
}

fn dto_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleDto> {
    Ok(ScheduleDto {
        id: row.get(0)?,
        session_id: row.get(1)?,
        kind: row.get(2)?,
        spec: row.get(3)?,
        prompt: row.get(4)?,
        status: row.get(5)?,
        related_task_id: row.get(6)?,
        created_at: row.get(7)?,
        last_run_at: row.get(8)?,
        next_run_at: row.get(9)?,
        timezone: row.get(10)?,
        error: row.get(11)?,
        title: row.get(12)?,
    })
}

// Short human title for a schedule: the first non-empty line of the prompt
// cut to `max_chars`, falling back to the spec when there is no prompt text.
// Shared by create() and the v23 backfill.
pub(crate) fn derive_title(prompt: Option<&str>, spec: &str, max_chars: usize) -> String {
    let line = prompt
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.is_empty() {
        return spec.trim().to_string();
    }
    line.chars().take(max_chars).collect::<String>().trim().to_string()
}

// Native notifications go through tauri-plugin-notification; on macOS the
// first notification triggers the OS permission prompt (SPEC 5.3). The trait
// lets tests inject a recording notifier.
pub trait Notifier {
    fn notify(&self, title: &str, body: &str) -> Result<()>;
}

pub struct TauriNotifier<'a> {
    app: &'a AppHandle,
}

impl<'a> TauriNotifier<'a> {
    pub fn new(app: &'a AppHandle) -> Self {
        TauriNotifier { app }
    }
}

impl Notifier for TauriNotifier<'_> {
    fn notify(&self, title: &str, body: &str) -> Result<()> {
        use tauri_plugin_notification::NotificationExt;
        self.app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
            .map_err(|e| AppError::Io(format!("notification: {e}")))
    }
}

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

pub fn system_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into())
}

// NULL timezone = the machine's IANA zone, resolved once per call.
fn resolve_timezone(tz: Option<&str>) -> Result<Tz> {
    let name = match tz.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => system_timezone_name(),
    };
    Tz::from_str(&name).map_err(|_| AppError::Invalid(format!("unknown IANA timezone: {name}")))
}

fn fmt_utc(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn parse_utc(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| AppError::Invalid(format!("invalid UTC timestamp {text:?}: {e}")))
}

// `run_at`/`notify` spec: RFC3339 (with offset or Z) or a naive
// "YYYY-MM-DDTHH:MM[:SS]" / "YYYY-MM-DD" interpreted in the schedule's zone.
fn parse_run_at(spec: &str, tz: Tz) -> Result<DateTime<Utc>> {
    let text = spec.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        return Ok(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDate::parse_from_str(text, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0).unwrap()))
        .map_err(|_| {
            AppError::Invalid(format!(
                "invalid run_at {spec:?}: expected ISO datetime like 2026-10-20T10:00:00"
            ))
        })?;
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            Ok(dt.with_timezone(&Utc))
        }
        chrono::LocalResult::None => Err(AppError::Invalid(format!(
            "{spec:?} does not exist in timezone {tz} (DST gap)"
        ))),
    }
}

// The cron crate numbers days of week 1=Sunday..7=Saturday; classic 5-field
// cron is 0=Sunday,1=Monday..6=Saturday (7 also Sunday). Remap the numeric
// tokens of the DOW field; names, steps and lists pass through.
fn dow_to_crate_ordinal(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut digits = String::new();
    let flush = |out: &mut String, digits: &mut String| {
        if digits.is_empty() {
            return;
        }
        match digits.parse::<u32>() {
            // 0 and 7 are Sunday -> crate 1; 1..6 shift by one.
            Ok(n) if n <= 7 => out.push_str(&(n % 7 + 1).to_string()),
            _ => out.push_str(digits),
        }
        digits.clear();
    };
    for c in field.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut out, &mut digits);
            out.push(c);
        }
    }
    flush(&mut out, &mut digits);
    out
}

// The cron crate parses 6/7 fields (seconds first); SPEC 5.3 schedules use
// the classic 5-field form, so a leading "0 " seconds field is prepended.
// Shorthand (@daily, @hourly, …) passes through untouched.
fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    let expr = expr.trim();
    if expr.starts_with('@') {
        return cron::Schedule::from_str(expr)
            .map_err(|e| AppError::Invalid(format!("invalid cron {expr:?}: {e}")));
    }
    let mut parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(AppError::Invalid(format!(
            "invalid 5-field cron {expr:?}: expected e.g. \"0 10 * * *\""
        )));
    }
    let dow = dow_to_crate_ordinal(parts[4]);
    parts[4] = &dow;
    let normalized = format!("0 {}", parts.join(" "));
    cron::Schedule::from_str(&normalized)
        .map_err(|e| AppError::Invalid(format!("invalid 5-field cron {expr:?}: {e}")))
}

// Next cron occurrence strictly after `base`, evaluated in the schedule's
// zone so DST transitions land on the right wall-clock instants.
fn next_cron_after(expr: &str, tz: Tz, base: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let schedule = parse_cron(expr)?;
    let base_local = base.with_timezone(&tz);
    let next = schedule
        .after(&base_local)
        .next()
        .ok_or_else(|| AppError::Invalid(format!("cron {expr:?} has no occurrence after {base_local}")))?;
    Ok(next.with_timezone(&Utc))
}

// The next fire instant for an active schedule of the given kind, or None
// when it never fires again (done). Errors on an invalid spec.
fn compute_next(kind: &str, spec: &str, tz: Tz, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    match kind {
        "run_at" | "notify" => Ok(Some(parse_run_at(spec, tz)?)),
        "cron" => Ok(Some(next_cron_after(spec, tz, now)?)),
        _ => Ok(None),
    }
}

fn validate_kind(kind: &str) -> Result<()> {
    if KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown schedule kind: {kind} (expected run_at|cron|notify)"
        )))
    }
}

pub struct CreateIn<'a> {
    pub kind: &'a str,
    pub spec: &'a str,
    pub prompt: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub session_id: Option<i64>,
    pub related_task_id: Option<i64>,
    pub title: Option<&'a str>,
}

pub fn create(conn: &Connection, input: &CreateIn<'_>, now: DateTime<Utc>) -> Result<ScheduleDto> {
    validate_kind(input.kind)?;
    let tz = resolve_timezone(input.timezone)?;
    let next = compute_next(input.kind, input.spec, tz, now)?.unwrap();
    if let Some(sid) = input.session_id {
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM sessions WHERE id = ?1",
            params![sid],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(AppError::Invalid(format!("session {sid} not found")));
        }
    }
    let title = input
        .title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| derive_title(input.prompt, input.spec, 60));
    conn.execute(
        "INSERT INTO schedules (session_id, kind, spec, prompt, status, related_task_id, created_at, next_run_at, timezone, title)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, ?9)",
        params![
            input.session_id,
            input.kind,
            input.spec,
            input.prompt,
            input.related_task_id,
            fmt_utc(now),
            fmt_utc(next),
            input.timezone.map(str::trim).filter(|s| !s.is_empty()),
            title,
        ],
    )?;
    schedule_dto(conn, conn.last_insert_rowid())
}

fn schedule_dto(conn: &Connection, id: i64) -> Result<ScheduleDto> {
    conn.query_row(
        &format!("SELECT {COLS} FROM schedules WHERE id = ?1"),
        params![id],
        dto_from_row,
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("schedule {id} not found")))
}

pub fn list(conn: &Connection) -> Result<Vec<ScheduleDto>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM schedules ORDER BY COALESCE(next_run_at, '9999'), id"
    ))?;
    let rows = stmt.query_map([], dto_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn set_status_and_next(
    conn: &Connection,
    id: i64,
    status: &str,
    next: Option<DateTime<Utc>>,
) -> Result<ScheduleDto> {
    let updated = conn.execute(
        "UPDATE schedules SET status = ?1, next_run_at = ?2, error = NULL WHERE id = ?3",
        params![status, next.map(fmt_utc), id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("schedule {id} not found")));
    }
    schedule_dto(conn, id)
}

pub fn pause(conn: &Connection, id: i64) -> Result<ScheduleDto> {
    set_status_and_next(conn, id, "paused", None)
}

pub fn resume(conn: &Connection, id: i64, now: DateTime<Utc>) -> Result<ScheduleDto> {
    let row: (String, String, Option<String>) = conn
        .query_row(
            "SELECT kind, spec, timezone FROM schedules WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid(format!("schedule {id} not found")))?;
    let tz = resolve_timezone(row.2.as_deref())?;
    let next = compute_next(&row.0, &row.1, tz, now)?
        .ok_or_else(|| AppError::Invalid(format!("schedule {id} is finished and cannot be resumed")))?;
    set_status_and_next(conn, id, "active", Some(next))
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let deleted = conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
    if deleted != 1 {
        return Err(AppError::Invalid(format!("schedule {id} not found")));
    }
    Ok(())
}

// Click-through from the schedules panel: the session whose chat the UI
// opens. The schedule's own session while it exists; otherwise a fresh one
// titled after the schedule, linked back so later opens and fires reuse it
// (same rule as ensure_run_session at fire time).
pub fn open_session(conn: &Connection, id: i64) -> Result<i64> {
    let s = schedule_dto(conn, id)?;
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
    let label = s.title.clone().unwrap_or_else(|| s.spec.clone());
    let session = harness::create_session(conn, Some(&format!("Расписание: {label}")))?;
    conn.execute(
        "UPDATE schedules SET session_id = ?1 WHERE id = ?2",
        params![session.id, id],
    )?;
    Ok(session.id)
}

// Engine CRUD used by the schedules UI slice (later): edit spec/prompt/tz/status.
#[allow(dead_code)]
pub struct UpdateIn<'a> {
    pub spec: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub status: Option<&'a str>,
}

#[allow(dead_code)]
pub fn update(conn: &Connection, id: i64, patch: &UpdateIn<'_>, now: DateTime<Utc>) -> Result<ScheduleDto> {
    let current = schedule_dto(conn, id)?;
    let spec = patch.spec.unwrap_or(&current.spec).to_string();
    let prompt = match patch.prompt {
        Some(p) => Some(p.to_string()),
        None => current.prompt.clone(),
    };
    let timezone = match patch.timezone {
        Some(t) => Some(t.trim().to_string()),
        None => current.timezone.clone(),
    };
    let status = patch.status.unwrap_or(&current.status).to_string();
    if !["active", "paused", "done", "error"].contains(&status.as_str()) {
        return Err(AppError::Invalid(format!("unknown schedule status: {status}")));
    }
    let tz = resolve_timezone(timezone.as_deref())?;
    let next = match status.as_str() {
        "active" => compute_next(&current.kind, &spec, tz, now)?,
        "paused" => None,
        // done/error keep their stored next_run_at unless the spec changed
        _ => {
            if patch.spec.is_some() || patch.timezone.is_some() {
                compute_next(&current.kind, &spec, tz, now)?
            } else {
                current.next_run_at.as_deref().map(parse_utc).transpose()?
            }
        }
    };
    let updated = conn.execute(
        "UPDATE schedules SET spec = ?1, prompt = ?2, timezone = ?3, status = ?4, next_run_at = ?5, error = NULL WHERE id = ?6",
        params![spec, prompt, timezone, status, next.map(fmt_utc), id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("schedule {id} not found")));
    }
    schedule_dto(conn, id)
}

fn mark_done(conn: &Connection, id: i64, now: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "UPDATE schedules SET status = 'done', last_run_at = ?1, next_run_at = NULL, error = NULL, retry_count = 0 WHERE id = ?2",
        params![fmt_utc(now), id],
    )?;
    Ok(())
}

fn mark_error(conn: &Connection, id: i64, message: &str) -> Result<()> {
    conn.execute(
        "UPDATE schedules SET status = 'error', error = ?1 WHERE id = ?2",
        params![message, id],
    )?;
    Ok(())
}

fn due_ids(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<i64>> {
    // Rate limit: a schedule whose last run started strictly less than
    // MIN_RUN_INTERVAL_SECONDS ago is skipped even when next_run_at is due
    // (RFC3339 UTC strings compare in chronological order; at exactly the
    // 60 s boundary the run is allowed, which is what the retry delay needs).
    let mut stmt = conn.prepare(
        "SELECT id FROM schedules
         WHERE status = 'active' AND next_run_at IS NOT NULL AND next_run_at <= ?1
           AND (last_run_at IS NULL OR last_run_at <= ?2)
         ORDER BY next_run_at, id",
    )?;
    let rows = stmt.query_map(
        params![fmt_utc(now), fmt_utc(now - chrono::Duration::seconds(MIN_RUN_INTERVAL_SECONDS))],
        |r| r.get(0),
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    let title = format!("Расписание · {} · {}", s.kind, s.spec);
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
            notifier.notify("Project S", &body)?;
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

static THREAD_STARTED: AtomicBool = AtomicBool::new(false);

// Starts the background tick thread once per process: an immediate first
// tick performs the startup catch-up (SPEC 5.3), then a tick every 30 s.
// The thread waits for the working dir to be configured and stops with the
// process. LaunchAgent autostart (separate slice) only needs to launch the
// app — this first tick is the catch-up seam.
pub fn start(app: AppHandle) {
    if THREAD_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        loop {
            let state = app.state::<AppState>();
            if state.working_dir.lock().unwrap().is_some() {
                let notifier = TauriNotifier::new(&app);
                let runner = TauriSessionRunner::new(&app);
                let guard = state.db.lock().unwrap();
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
    use crate::db::schema;
    use crate::harness::{self, create_session};
    use rusqlite::Connection;
    use std::sync::Mutex;

    struct RecordingNotifier {
        sent: Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    impl RecordingNotifier {
        fn new() -> Self {
            RecordingNotifier {
                sent: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
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

    struct RecordingRunner {
        runs: Mutex<Vec<(i64, String)>>,
        fail: bool,
    }

    impl RecordingRunner {
        fn new() -> Self {
            RecordingRunner {
                runs: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
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

    fn setup() -> Connection {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn
    }

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    fn moscow() -> Tz {
        Tz::from_str("Europe/Moscow").unwrap()
    }

    fn create_session_row(conn: &Connection) -> i64 {
        create_session(conn, Some("sched")).unwrap().id
    }

    #[test]
    fn cron_next_occurrence_uses_the_fixed_timezone() {
        // 10:00 MSK = 07:00 UTC (Moscow has no DST).
        let next = next_cron_after("0 10 * * *", moscow(), utc("2026-10-16T08:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-17T07:00:00Z");
        // Strictly after the base instant, not the same minute.
        let next = next_cron_after("0 10 * * *", moscow(), utc("2026-10-16T07:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-17T07:00:00Z");
        // Weekday grid: Fri 16 Oct 2026 18:30 MSK = 15:30 UTC.
        let next = next_cron_after("30 18 * * 1-5", moscow(), utc("2026-10-16T00:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-16T15:30:00Z");
        // DST: 09:00 America/New_York; EDT (UTC-4) before the 1 Nov 2026
        // fall-back, EST (UTC-5) after it.
        let ny = Tz::from_str("America/New_York").unwrap();
        let next = next_cron_after("0 9 * * *", ny, utc("2026-10-30T12:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-10-30T13:00:00Z");
        let next = next_cron_after("0 9 * * *", ny, utc("2026-11-02T12:00:00Z")).unwrap();
        assert_eq!(fmt_utc(next), "2026-11-02T14:00:00Z");
        assert!(next_cron_after("bogus", moscow(), utc("2026-10-16T00:00:00Z")).is_err());
        // Exactly 5 fields (or @shorthand): the seconds-first form is rejected.
        assert!(next_cron_after("0 0 10 * * *", moscow(), utc("2026-10-16T00:00:00Z")).is_err());
        let next = next_cron_after("@daily", moscow(), utc("2026-10-16T00:00:00Z")).unwrap();
        // midnight MSK = 21:00 UTC the previous day
        assert_eq!(fmt_utc(next), "2026-10-16T21:00:00Z");
    }

    #[test]
    fn create_computes_next_run_and_validates_input() {
        let conn = setup();
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-20T10:00:00",
                prompt: Some("check rates"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: Some(7), title: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(s.status, "active");
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-20T07:00:00Z"));
        assert_eq!(s.timezone.as_deref(), Some("Europe/Moscow"));
        assert_eq!(s.related_task_id, Some(7));

        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-17T07:00:00Z"));

        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "bogus", spec: "x", prompt: None, timezone: None, session_id: None, related_task_id: None, title: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "run_at", spec: "tomorrow", prompt: None, timezone: None, session_id: None, related_task_id: None, title: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: None, timezone: Some("Mars/Olympus"), session_id: None, related_task_id: None, title: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: None, timezone: None, session_id: Some(999), related_task_id: None, title: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
    }

    #[test]
    fn pause_stops_firing_and_resume_recomputes_from_now() {
        let conn = setup();
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
            },
            now,
        )
        .unwrap();

        let paused = pause(&conn, s.id).unwrap();
        assert_eq!(paused.status, "paused");
        assert_eq!(paused.next_run_at, None);
        let fired = tick(
            &conn,
            utc("2026-10-17T09:00:00Z"),
            &RecordingNotifier::new(),
            &RecordingRunner::new(),
        )
        .unwrap();
        assert!(!fired, "paused schedule fired");

        let resumed = resume(&conn, s.id, utc("2026-10-18T09:00:00Z")).unwrap();
        assert_eq!(resumed.status, "active");
        assert_eq!(resumed.next_run_at.as_deref(), Some("2026-10-19T07:00:00Z"));
        assert!(matches!(resume(&conn, 999, now), Err(AppError::Invalid(_))));
    }

    #[test]
    fn tick_fires_due_schedules_once_and_advances_them() {
        let conn = setup();
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let notify = create(
            &conn,
            &CreateIn {
                kind: "notify",
                spec: "2026-10-16T10:00:00",
                prompt: Some("call the client"),
                timezone: Some("Europe/Moscow"),
                session_id: None,
                related_task_id: None, title: None,
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
                session_id: Some(session),
                related_task_id: None, title: None,
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
                session_id: Some(session),
                related_task_id: None, title: None,
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
            vec![("Project S".to_string(), "call the client".to_string())]
        );
        assert_eq!(
            *runner.runs.lock().unwrap(),
            vec![
                (session, "do the thing".to_string()),
                (session, "daily".to_string()),
            ]
        );

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
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        // run_at whose session vanished: the runner reports the failure.
        let run_at = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
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
                session_id: None,
                related_task_id: None, title: None,
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
        let session = create_session_row(&conn);
        let cron = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
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
        let session = create_session_row(&conn);
        let cron = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
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
        let session = create_session_row(&conn);
        let run_at = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
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
    fn deleting_a_session_keeps_its_schedule_with_a_null_session() {
        let conn = setup();
        let session = create_session_row(&conn);
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();

        harness::delete_session(&conn, session).unwrap();
        let after = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(after.session_id, None);
        assert_eq!(after.status, "active");
    }

    #[test]
    fn firing_a_schedule_whose_session_was_deleted_creates_a_fresh_one() {
        let conn = setup();
        let old = create_session_row(&conn);
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(old),
                related_task_id: None, title: None,
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        harness::delete_session(&conn, old).unwrap();

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
        assert_eq!(
            title.as_deref(),
            Some("Расписание · run_at · 2026-10-16T10:00:00")
        );
        // The fresh session has its harness chat row: the run bridge needs it.
        assert!(harness::session_chat_id(&conn, fresh).is_ok());
    }

    #[test]
    fn firing_a_run_at_created_without_a_session_creates_one() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-16T10:00:00",
                prompt: Some("go"),
                timezone: Some("Europe/Moscow"),
                session_id: None,
                related_task_id: None, title: None,
            },
            utc("2026-10-16T08:00:00Z"),
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
        let session = create_session_row(&conn);
        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
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
        assert_eq!(
            *runner.runs.lock().unwrap(),
            vec![(session, "daily".to_string())]
        );
        let after = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(after.session_id, Some(session));
    }

    #[test]
    fn update_accepts_rfc3339_specs_and_round_trips_stored_timestamps() {
        let conn = setup();
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-20T10:00:00",
                prompt: Some("x"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
            },
            now,
        )
        .unwrap();
        let updated = update(
            &conn,
            s.id,
            &UpdateIn {
                spec: Some("2026-10-22T10:00:00+03:00"),
                prompt: None,
                timezone: None,
                status: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(updated.next_run_at.as_deref(), Some("2026-10-22T07:00:00Z"));
        // The stored UTC form parses back for the done/error passthrough path.
        assert_eq!(
            fmt_utc(parse_utc(updated.next_run_at.as_ref().unwrap()).unwrap()),
            "2026-10-22T07:00:00Z"
        );
    }

    #[test]
    fn update_edits_spec_prompt_and_status() {        let conn = setup();
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-20T10:00:00",
                prompt: Some("old"),
                timezone: Some("Europe/Moscow"),
                session_id: Some(session),
                related_task_id: None, title: None,
            },
            now,
        )
        .unwrap();
        let updated = update(
            &conn,
            s.id,
            &UpdateIn {
                spec: Some("2026-10-21T10:00:00"),
                prompt: Some("new"),
                timezone: None,
                status: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(updated.spec, "2026-10-21T10:00:00");
        assert_eq!(updated.prompt.as_deref(), Some("new"));
        assert_eq!(updated.next_run_at.as_deref(), Some("2026-10-21T07:00:00Z"));

        let paused = update(
            &conn,
            s.id,
            &UpdateIn { spec: None, prompt: None, timezone: None, status: Some("paused") },
            now,
        )
        .unwrap();
        assert_eq!((paused.status.as_str(), paused.next_run_at.as_deref()), ("paused", None));
        assert!(matches!(
            update(&conn, s.id, &UpdateIn { spec: None, prompt: None, timezone: None, status: Some("bogus") }, now),
            Err(AppError::Invalid(_))
        ));
        assert!(delete(&conn, s.id).is_ok());
        assert!(matches!(delete(&conn, s.id), Err(AppError::Invalid(_))));
    }

    #[test]
    fn list_orders_by_next_run_and_exposes_the_origin_fields() {
        let conn = setup();
        let session = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let later = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-20T10:00:00", prompt: Some("later"), timezone: Some("Europe/Moscow"), session_id: Some(session), related_task_id: Some(3), title: None },
            now,
        )
        .unwrap();
        let sooner = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-18T10:00:00", prompt: Some("sooner"), timezone: Some("Europe/Moscow"), session_id: Some(session), related_task_id: None, title: None },
            now,
        )
        .unwrap();
        let items = list(&conn).unwrap();
        assert_eq!(items.iter().map(|s| s.id).collect::<Vec<_>>(), vec![sooner.id, later.id]);
        assert_eq!(items[1].related_task_id, Some(3));
        assert_eq!(items[1].prompt.as_deref(), Some("later"));
        assert_eq!(items[1].title.as_deref(), Some("later"));
    }

    #[test]
    fn create_takes_an_explicit_title_or_derives_one_from_prompt_or_spec() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("  \n  проверить цены на сайте\nлишняя строка\n"), timezone: None, session_id: None, related_task_id: None, title: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref(), Some("проверить цены на сайте"));

        // A long first line is cut to 60 chars.
        let long = "д".repeat(80);
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "@daily", prompt: Some(&long), timezone: None, session_id: None, related_task_id: None, title: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref().unwrap().chars().count(), 60);

        // No prompt: the spec becomes the title.
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: None, timezone: None, session_id: None, related_task_id: None, title: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref(), Some("0 10 * * *"));

        // An explicit title wins and is trimmed.
        let s = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-20T10:00:00", prompt: Some("x"), timezone: None, session_id: None, related_task_id: None, title: Some("  Утренняя сводка  ") },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref(), Some("Утренняя сводка"));
    }

    #[test]
    fn open_session_reuses_the_linked_session_or_creates_a_titled_one() {
        let conn = setup();
        let session = create_session_row(&conn);
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("daily"), timezone: Some("Europe/Moscow"), session_id: Some(session), related_task_id: None, title: Some("Ежедневный отчёт") },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        assert_eq!(open_session(&conn, s.id).unwrap(), session);

        // The session was deleted: a fresh one titled after the schedule is
        // created and linked back, and later opens reuse it. (The fresh rowid
        // may equal the deleted one — SQLite reuses rowids without
        // AUTOINCREMENT — so the link and the title carry the assertion.)
        harness::delete_session(&conn, session).unwrap();
        let fresh = open_session(&conn, s.id).unwrap();
        let after = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(after.session_id, Some(fresh));
        let title: Option<String> = conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![fresh], |r| r.get(0))
            .unwrap();
        assert_eq!(title.as_deref(), Some("Расписание: Ежедневный отчёт"));
        assert_eq!(open_session(&conn, s.id).unwrap(), fresh);

        assert!(matches!(open_session(&conn, 999), Err(AppError::Invalid(_))));
    }
}

