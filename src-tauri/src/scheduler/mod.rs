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
use crate::sync_util::lock_recover;
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

// Q.md item 7: the notification action type that makes a completion
// notification clickable; the click handler in lib.rs reports the body
// click as "tap" and the button as "open".
pub const CLICK_ACTION_TYPE: &str = "schedule";

const COLS: &str = "id, session_id, kind, spec, prompt, status, related_task_id, created_at, last_run_at, next_run_at, timezone, error, title, unread_at";

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
    pub unread_at: Option<String>,
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
        unread_at: row.get(13)?,
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

    // Q.md item 7: a scheduled run's completion notification carries the
    // schedule and session ids as extras and references the registered
    // click action type, so clicking it opens the schedule's chat. The
    // default drops the ids (plain delivery); tests keep the plain path.
    fn notify_completion(
        &self,
        title: &str,
        body: &str,
        schedule_id: i64,
        session_id: i64,
    ) -> Result<()> {
        let _ = (schedule_id, session_id);
        self.notify(title, body)
    }
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

    fn notify_completion(
        &self,
        title: &str,
        body: &str,
        schedule_id: i64,
        session_id: i64,
    ) -> Result<()> {
        use tauri_plugin_notification::NotificationExt;
        self.app
            .notification()
            .builder()
            .title(title)
            .body(body)
            .action_type_id(CLICK_ACTION_TYPE)
            .extra("scheduleId", schedule_id)
            .extra("sessionId", session_id)
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

#[cfg(test)]
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
    pub related_task_id: Option<i64>,
    pub title: Option<&'a str>,
    // The Harness chat the agent is creating the schedule from, when any.
    // Q.md item 1: a brand-new chat that only holds the single user turn that
    // asked for the schedule becomes the schedule's own chat; once it has more
    // history the schedule gets a separate chat as before.
    pub session_id: Option<i64>,
}

// True when `session_id` still names a Harness session whose chat holds exactly
// one user turn: the fresh chat that just asked for the schedule. A missing
// session or chat (deleted meanwhile) or any extra history makes it not fresh,
// so the caller falls back to a dedicated new chat.
fn creating_session_is_fresh(conn: &Connection, session_id: i64) -> Result<bool> {
    let Ok(chat_id) = harness::session_chat_id(conn, session_id) else {
        return Ok(false);
    };
    let user_turns: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE chat_id = ?1 AND role = 'user'",
        params![chat_id],
        |r| r.get(0),
    )?;
    Ok(user_turns == 1)
}

// Q.md items 4/5: every new schedule gets its own dedicated Harness chat,
// titled after the schedule, even when the agent creates it from another
// chat (that chat keeps its own context). Q.md item 1 refines this: when the
// creating chat is still empty apart from the one user turn that asked for the
// schedule, that chat becomes the schedule's chat instead of a new one. Only
// new schedules follow this rule; existing rows keep their session links
// untouched.
pub fn create(conn: &Connection, input: &CreateIn<'_>, now: DateTime<Utc>) -> Result<ScheduleDto> {
    validate_kind(input.kind)?;
    let tz = resolve_timezone(input.timezone)?;
    let next = compute_next(input.kind, input.spec, tz, now)?.unwrap();
    let title = input
        .title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| derive_title(input.prompt, input.spec, 60));
    let session_id = match input.session_id {
        Some(sid) if creating_session_is_fresh(conn, sid)? => {
            // Reuse the creating chat; give it the schedule title only while it
            // is still untitled, never overwriting a title the user set.
            conn.execute(
                "UPDATE sessions SET title = ?1
                 WHERE id = ?2 AND (title IS NULL OR trim(title) = '')",
                params![title, sid],
            )?;
            sid
        }
        _ => harness::create_session(conn, Some(&title))?.id,
    };
    conn.execute(
        "INSERT INTO schedules (session_id, kind, spec, prompt, status, related_task_id, created_at, next_run_at, timezone, title)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, ?9)",
        params![
            session_id,
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

// Q.md item 5: renaming a schedule renames its linked chat too, always
// overwriting a manual chat title. A detached or deleted session is skipped
// without failing: open_session and ensure_run_session recreate it titled
// after the schedule later.
pub fn rename(conn: &Connection, id: i64, title: &str) -> Result<ScheduleDto> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::Invalid("schedule title is empty".into()));
    }
    let updated = conn.execute(
        "UPDATE schedules SET title = ?1 WHERE id = ?2",
        params![title, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("schedule {id} not found")));
    }
    let session_id: Option<i64> = conn.query_row(
        "SELECT session_id FROM schedules WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    if let Some(sid) = session_id {
        let _ = harness::rename_session(conn, sid, title);
    }
    schedule_dto(conn, id)
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

// Q.md: a schedule and the chat it was created in are removed together. When
// the schedule has a linked session, deleting that session cascades to all its
// schedules (this one included) in one transaction; the trailing DELETE covers
// a schedule with no session (or one the cascade already removed).
pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    let s = schedule_dto(conn, id)?;
    if let Some(sid) = s.session_id {
        harness::delete_session(conn, sid)?;
    }
    conn.execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
    Ok(())
}

// Click-through from the schedules panel: the session whose chat the UI
// opens. The schedule's own session while it exists; otherwise a fresh one
// titled after the schedule, linked back so later opens and fires reuse it
// (same rule as ensure_run_session at fire time). Opening is also the read
// mark (Q.md item 7): the unread stamp clears on every open path, and the
// schedule_open command emits "schedules-changed" so the pulse disappears.
pub fn open_session(conn: &Connection, id: i64) -> Result<i64> {
    let s = schedule_dto(conn, id)?;
    if let Some(sid) = s.session_id {
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM sessions WHERE id = ?1",
            params![sid],
            |r| r.get(0),
        )?;
        if exists > 0 {
            conn.execute("UPDATE schedules SET unread_at = NULL WHERE id = ?1", params![id])?;
            return Ok(sid);
        }
    }
    let label = s.title.clone().unwrap_or_else(|| s.spec.clone());
    let session = harness::create_session(conn, Some(&label))?;
    conn.execute(
        "UPDATE schedules SET session_id = ?1, unread_at = NULL WHERE id = ?2",
        params![session.id, id],
    )?;
    Ok(session.id)
}

// Q.md item 7: a finished scheduled agent run (run_at/cron) marks its
// schedule unread and yields the completion notification payload. The
// schedule is the one that most recently fired into this session; `notify`
// schedules already notified at fire time and never re-notify here. The
// unread stamp is written before the caller delivers the notification, so
// a user with notifications disabled still gets the visual pulse.
pub struct CompletionNotice {
    pub schedule_id: i64,
    pub session_id: i64,
    pub title: String,
    pub body: String,
}

pub fn mark_run_completion(
    conn: &Connection,
    session_id: i64,
    answer: &str,
    finish_reason: &str,
    now: DateTime<Utc>,
) -> Result<Option<CompletionNotice>> {
    let row: Option<(i64, Option<String>, String, String, Option<String>)> = conn
        .query_row(
            "SELECT id, title, kind, spec, prompt FROM schedules
             WHERE session_id = ?1
             ORDER BY last_run_at DESC, id DESC LIMIT 1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, title, kind, spec, prompt)) = row else {
        return Ok(None);
    };
    if kind == "notify" {
        return Ok(None);
    }
    let title = title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| derive_title(prompt.as_deref(), &spec, 60));
    let body = completion_body(answer, finish_reason);
    conn.execute(
        "UPDATE schedules SET unread_at = ?1 WHERE id = ?2",
        params![fmt_utc(now), id],
    )?;
    Ok(Some(CompletionNotice {
        schedule_id: id,
        session_id,
        title,
        body,
    }))
}

// Notification body for a finished run: the answer's first 120 chars on a
// normal finish; on any other finish reason the answer already carries the
// stop notice or error text, and an empty answer falls back to the reason.
fn completion_body(answer: &str, finish_reason: &str) -> String {
    let text = answer.trim();
    if matches!(finish_reason, "finish" | "stop") || !text.is_empty() {
        return truncate_chars(text, 120);
    }
    format!("Сбой выполнения: {finish_reason}")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

// Engine CRUD used by the schedules UI slice (later): edit spec/prompt/tz/status.
#[cfg(test)]
pub struct UpdateIn<'a> {
    pub spec: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub status: Option<&'a str>,
}

#[cfg(test)]
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

    // Appends one user turn to a session's harness chat, the way a real chat
    // run persists the message that triggered the agent.
    fn add_user_turn(conn: &Connection, session_id: i64, text: &str) {
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

    fn session_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap()
    }

    fn session_title(conn: &Connection, session_id: i64) -> Option<String> {
        conn.query_row(
            "SELECT title FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap()
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
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-20T10:00:00",
                prompt: Some("check rates"),
                timezone: Some("Europe/Moscow"),
                related_task_id: Some(7), title: None, session_id: None
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
                related_task_id: None, title: None, session_id: None
            },
            now,
        )
        .unwrap();
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-17T07:00:00Z"));

        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "bogus", spec: "x", prompt: None, timezone: None, related_task_id: None, title: None, session_id: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "run_at", spec: "tomorrow", prompt: None, timezone: None, related_task_id: None, title: None, session_id: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            create(
                &conn,
                &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: None, timezone: Some("Mars/Olympus"), related_task_id: None, title: None, session_id: None },
                now,
            ),
            Err(AppError::Invalid(_))
        ));
    }

    #[test]
    fn create_binds_the_creating_session_while_it_is_fresh() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let sid = create_session(&conn, None).unwrap().id;
        add_user_turn(&conn, sid, "сделай расписание");

        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: None,
                related_task_id: None,
                title: None,
                session_id: Some(sid),
            },
            now,
        )
        .unwrap();

        assert_eq!(s.session_id, Some(sid), "the fresh creating chat becomes the schedule chat");
        assert_eq!(session_count(&conn), 1, "no extra session is created");
        assert_eq!(session_title(&conn, sid).as_deref(), Some("daily"), "an untitled session takes the derived schedule title");
    }

    #[test]
    fn create_keeps_a_separate_chat_once_the_creating_session_has_history() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let sid = create_session(&conn, Some("chat")).unwrap().id;
        add_user_turn(&conn, sid, "первый вопрос");
        add_user_turn(&conn, sid, "сделай расписание");

        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: None,
                related_task_id: None,
                title: None,
                session_id: Some(sid),
            },
            now,
        )
        .unwrap();

        let bound = s.session_id.expect("schedule has a session");
        assert_ne!(bound, sid, "a chat with history keeps its own context; the schedule gets a new chat");
        assert_eq!(session_count(&conn), 2, "a dedicated new session was created");
    }

    #[test]
    fn create_falls_back_to_a_new_session_when_the_bound_session_is_gone() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: None,
                related_task_id: None,
                title: None,
                session_id: Some(999),
            },
            now,
        )
        .unwrap();

        let bound = s.session_id.expect("schedule has a session");
        assert_eq!(session_count(&conn), 1, "a new session was created for the missing one");
        assert_eq!(session_title(&conn, bound).as_deref(), Some("daily"));
    }

    #[test]
    fn create_never_overwrites_an_existing_title_when_binding() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let sid = create_session(&conn, Some("моё имя")).unwrap().id;
        add_user_turn(&conn, sid, "сделай расписание");

        let s = create(
            &conn,
            &CreateIn {
                kind: "cron",
                spec: "0 10 * * *",
                prompt: Some("daily"),
                timezone: None,
                related_task_id: None,
                title: None,
                session_id: Some(sid),
            },
            now,
        )
        .unwrap();

        assert_eq!(s.session_id, Some(sid), "still binds the fresh chat");
        assert_eq!(session_title(&conn, sid).as_deref(), Some("моё имя"), "an existing title is left untouched");
    }

    #[test]
    fn pause_stops_firing_and_resume_recomputes_from_now() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
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
    fn deleting_a_session_deletes_its_schedules() {
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

        harness::delete_session(&conn, s.session_id.unwrap()).unwrap();
        // Q.md: the schedule is removed with its chat, not left orphaned.
        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM schedules WHERE id = ?1", params![s.id], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn deleting_a_schedule_deletes_its_linked_session() {
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

        delete(&conn, s.id).unwrap();
        let sessions: i64 = conn
            .query_row("SELECT count(*) FROM sessions WHERE id = ?1", params![own], |r| r.get(0))
            .unwrap();
        let schedules: i64 = conn
            .query_row("SELECT count(*) FROM schedules WHERE id = ?1", params![s.id], |r| r.get(0))
            .unwrap();
        assert_eq!((sessions, schedules), (0, 0));
    }

    #[test]
    fn deleting_a_schedule_without_a_session_removes_only_the_schedule() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn {
                kind: "notify",
                spec: "2026-10-16T10:00:00",
                prompt: Some("ping"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
            },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        // A detached schedule (TTL purge nulled the link) has no chat to remove.
        conn.execute(
            "UPDATE schedules SET session_id = NULL WHERE id = ?1",
            params![s.id],
        )
        .unwrap();

        delete(&conn, s.id).unwrap();
        let schedules: i64 = conn
            .query_row("SELECT count(*) FROM schedules WHERE id = ?1", params![s.id], |r| r.get(0))
            .unwrap();
        assert_eq!(schedules, 0);
    }

    #[test]
    fn deleting_a_chat_removes_every_schedule_created_in_it() {
        let conn = setup();
        let session = create_session_row(&conn);
        let a = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("a"), timezone: Some("Europe/Moscow"), related_task_id: None, title: None, session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        let b = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-20T10:00:00", prompt: Some("b"), timezone: Some("Europe/Moscow"), related_task_id: None, title: None, session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        // Two schedules sharing one chat (an old row created before the
        // dedicated-chat rule): deleting that chat removes both.
        conn.execute(
            "UPDATE schedules SET session_id = ?1 WHERE id IN (?2, ?3)",
            params![session, a.id, b.id],
        )
        .unwrap();

        harness::delete_session(&conn, session).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT count(*) FROM schedules WHERE session_id = ?1", params![session], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
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
    fn update_accepts_rfc3339_specs_and_round_trips_stored_timestamps() {
        let conn = setup();
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-20T10:00:00",
                prompt: Some("x"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
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
        let now = utc("2026-10-16T08:00:00Z");
        let s = create(
            &conn,
            &CreateIn {
                kind: "run_at",
                spec: "2026-10-20T10:00:00",
                prompt: Some("old"),
                timezone: Some("Europe/Moscow"),
                related_task_id: None, title: None, session_id: None
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
        let now = utc("2026-10-16T08:00:00Z");
        let later = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-20T10:00:00", prompt: Some("later"), timezone: Some("Europe/Moscow"), related_task_id: Some(3), title: None, session_id: None },
            now,
        )
        .unwrap();
        let sooner = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-18T10:00:00", prompt: Some("sooner"), timezone: Some("Europe/Moscow"), related_task_id: None, title: None, session_id: None },
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
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("  \n  проверить цены на сайте\nлишняя строка\n"), timezone: None, related_task_id: None, title: None, session_id: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref(), Some("проверить цены на сайте"));

        // A long first line is cut to 60 chars.
        let long = "д".repeat(80);
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "@daily", prompt: Some(&long), timezone: None, related_task_id: None, title: None, session_id: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref().unwrap().chars().count(), 60);

        // No prompt: the spec becomes the title.
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: None, timezone: None, related_task_id: None, title: None, session_id: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref(), Some("0 10 * * *"));

        // An explicit title wins and is trimmed.
        let s = create(
            &conn,
            &CreateIn { kind: "run_at", spec: "2026-10-20T10:00:00", prompt: Some("x"), timezone: None, related_task_id: None, title: Some("  Утренняя сводка  "), session_id: None },
            now,
        )
        .unwrap();
        assert_eq!(s.title.as_deref(), Some("Утренняя сводка"));

        // Q.md item 5: the dedicated chat carries the same title.
        let title: Option<String> = conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![s.session_id.unwrap()], |r| r.get(0))
            .unwrap();
        assert_eq!(title.as_deref(), Some("Утренняя сводка"));
    }

    #[test]
    fn create_gives_each_schedule_its_own_chat_titled_after_the_schedule() {
        let conn = setup();
        let talking_chat = create_session_row(&conn);
        let now = utc("2026-10-16T08:00:00Z");
        let a = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 */2 * * *", prompt: Some("Привет каждые 2 часа (9:00–17:00)"), timezone: None, related_task_id: None, title: None, session_id: None },
            now,
        )
        .unwrap();
        let b = create(
            &conn,
            &CreateIn { kind: "notify", spec: "2026-10-20T10:00:00", prompt: Some("ping"), timezone: None, related_task_id: None, title: None, session_id: None },
            now,
        )
        .unwrap();
        let (sa, sb) = (
            a.session_id.expect("schedule a owns a chat"),
            b.session_id.expect("schedule b owns a chat"),
        );
        // Q.md item 4: distinct chats, never the chat the schedule came from.
        assert_ne!(sa, sb);
        assert_ne!(sa, talking_chat);
        assert_ne!(sb, talking_chat);
        // Q.md item 5: each chat title equals its schedule title.
        let titles: Vec<Option<String>> = conn
            .prepare("SELECT title FROM sessions WHERE id IN (?1, ?2) ORDER BY id")
            .unwrap()
            .query_map(params![sa, sb], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            titles,
            vec![
                Some("Привет каждые 2 часа (9:00–17:00)".to_string()),
                Some("ping".to_string()),
            ]
        );
    }

    #[test]
    fn rename_updates_the_schedule_and_renames_the_linked_chat_overriding_manual_titles() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("daily"), timezone: None, related_task_id: None, title: Some("Старое имя"), session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        let sid = s.session_id.unwrap();
        // A manual chat rename must be overwritten by the schedule rename.
        harness::rename_session(&conn, sid, "Пользователь переименовал сам").unwrap();

        let renamed = rename(&conn, s.id, "  Новое имя  ").unwrap();
        assert_eq!(renamed.title.as_deref(), Some("Новое имя"));
        let title: Option<String> = conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![sid], |r| r.get(0))
            .unwrap();
        assert_eq!(title.as_deref(), Some("Новое имя"));

        assert!(matches!(rename(&conn, s.id, "   "), Err(AppError::Invalid(_))));
        assert!(matches!(rename(&conn, 999, "x"), Err(AppError::Invalid(_))));
    }

    #[test]
    fn rename_survives_a_detached_or_deleted_linked_session() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("daily"), timezone: None, related_task_id: None, title: Some("Старое имя"), session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        // Detached link (TTL purge nulls it): the schedule title still updates.
        conn.execute(
            "UPDATE schedules SET session_id = NULL WHERE id = ?1",
            params![s.id],
        )
        .unwrap();
        assert_eq!(rename(&conn, s.id, "Новое имя").unwrap().title.as_deref(), Some("Новое имя"));
    }

    #[test]
    fn existing_rows_keep_their_session_links_when_a_new_schedule_is_created() {
        let conn = setup();
        let session = create_session_row(&conn);
        // A schedule created before the dedicated-chat rule (raw row with a
        // session link): the new rule applies only to new schedules.
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, prompt, status, created_at, next_run_at, timezone, title)
             VALUES (?1, 'cron', '0 10 * * *', 'daily', 'active', '2026-10-01T00:00:00Z', '2026-10-17T07:00:00Z', 'Europe/Moscow', 'старое')",
            params![session],
        )
        .unwrap();
        let fresh = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 12 * * *", prompt: Some("new"), timezone: None, related_task_id: None, title: None, session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        assert_ne!(fresh.session_id.unwrap(), session);
        let (old_sid, old_title): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT s.session_id, se.title FROM schedules s
                 JOIN sessions se ON se.id = s.session_id
                 WHERE s.title = 'старое'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(old_sid, Some(session));
        assert_eq!(old_title.as_deref(), Some("sched"));
    }

    #[test]
    fn open_session_reuses_the_linked_session_or_creates_a_titled_one() {
        let conn = setup();
        let s = create(
            &conn,
            &CreateIn { kind: "cron", spec: "0 10 * * *", prompt: Some("daily"), timezone: Some("Europe/Moscow"), related_task_id: None, title: Some("Ежедневный отчёт"), session_id: None },
            utc("2026-10-16T08:00:00Z"),
        )
        .unwrap();
        let own = s.session_id.expect("a new schedule owns its chat");
        assert_eq!(open_session(&conn, s.id).unwrap(), own);

        // The session was detached (a TTL purge nulls session_id without
        // deleting the schedule): a fresh one titled after the schedule is
        // created and linked back, and later opens reuse it. (Deleting the
        // session through delete_session would have removed the schedule too.)
        conn.execute(
            "UPDATE schedules SET session_id = NULL WHERE id = ?1",
            params![s.id],
        )
        .unwrap();
        harness::delete_session(&conn, own).unwrap();
        let fresh = open_session(&conn, s.id).unwrap();
        let after = schedule_dto(&conn, s.id).unwrap();
        assert_eq!(after.session_id, Some(fresh));
        let title: Option<String> = conn
            .query_row("SELECT title FROM sessions WHERE id = ?1", params![fresh], |r| r.get(0))
            .unwrap();
        // Q.md item 5: the recreated chat title is exactly the schedule title.
        assert_eq!(title.as_deref(), Some("Ежедневный отчёт"));
        assert_eq!(open_session(&conn, s.id).unwrap(), fresh);

        assert!(matches!(open_session(&conn, 999), Err(AppError::Invalid(_))));
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

    #[test]
    fn completion_notifications_fall_back_to_plain_delivery_in_recording_testers() {
        // The trait default drops the schedule/session ids and delivers a
        // plain notification, so every existing recording tester keeps the
        // completion path observable.
        let notifier = RecordingNotifier::new();
        notifier.notify_completion("t", "b", 1, 2).unwrap();
        assert_eq!(
            *notifier.sent.lock().unwrap(),
            vec![("t".to_string(), "b".to_string())]
        );
    }

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

