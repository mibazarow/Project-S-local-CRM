use crate::error::{AppError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const STATUSES: [&str; 3] = ["new", "progress", "done"];
pub const RECURRENCES: [&str; 4] = ["none", "daily", "weekly", "monthly"];
pub const MAX_CHECKLIST_ITEMS: usize = 100;
pub const DEFAULT_DUE_TIME: &str = "07:00";

// Kanban column order: new -> progress -> done.
const ORDER_SQL: &str = "ORDER BY CASE status WHEN 'new' THEN 0 WHEN 'progress' THEN 1 ELSE 2 END, position, id";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDto {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub status: String,
    pub comment: Option<String>,
    pub due_date: Option<String>,
    pub due_time: String,
    pub remind_at: Option<String>,
    pub position: f64,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub overdue: bool,
    pub is_priority: bool,
    pub checklist: Vec<ChecklistItem>,
    pub recurrence: String,
    // Wall-clock timer (SPEC 4.3): accumulated spent ms plus the active
    // start instant in epoch ms (UTC); started_at is set exactly while the
    // task sits in 'progress'. The UI derives the live total as
    // spentMs + (now - startedAt) while running.
    pub spent_ms: i64,
    pub started_at: Option<i64>,
}

// Distinguishes an explicit null (Some(None) -> clear) from an absent field (None -> keep).
mod opt_opt_str {
    use serde::de::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(Some)
    }
}

// Absent field leaves the value unchanged; explicit null clears it.
// is_priority/checklist/recurrence are NOT NULL columns: an explicit null is
// indistinguishable from absent and keeps the current value. due_time is
// NOT NULL too: null/absent keeps it, a set value must be HH:MM.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskPatch {
    pub title: Option<String>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub comment: Option<Option<String>>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub due_date: Option<Option<String>>,
    pub due_time: Option<String>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub remind_at: Option<Option<String>>,
    pub is_priority: Option<bool>,
    pub checklist: Option<Vec<ChecklistItem>>,
    pub recurrence: Option<String>,
}

pub fn tasks_list(conn: &Connection, project_id: i64) -> Result<Vec<TaskDto>> {
    ensure_project(conn, project_id)?;
    let now = now(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, status, comment, due_date, due_time, remind_at, position, created_at, closed_at,
                is_priority, checklist, recurrence, spent_ms, started_at
         FROM tasks WHERE project_id = ?1 {ORDER_SQL}"
    ))?;
    let mut rows = stmt.query(params![project_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let status: String = row.get(2)?;
        let due_date: Option<String> = row.get(4)?;
        let due_time: String = row.get(5)?;
        let overdue = is_overdue(&status, due_date.as_deref(), &due_time, &now);
        out.push(TaskDto {
            id: row.get(0)?,
            project_id,
            title: row.get(1)?,
            overdue,
            position: row.get(7)?,
            created_at: row.get(8)?,
            closed_at: row.get(9)?,
            comment: row.get(3)?,
            due_date,
            due_time,
            remind_at: row.get(6)?,
            status,
            is_priority: row.get::<_, i64>(10)? != 0,
            checklist: parse_checklist(&row.get::<_, String>(11)?)?,
            recurrence: row.get(12)?,
            spent_ms: row.get(13)?,
            started_at: row.get(14)?,
        });
    }
    Ok(out)
}

// Test convenience over task_create_with defaults; production entry points
// pass the upgrade fields explicitly.
#[cfg(test)]
pub fn task_create(
    conn: &Connection,
    project_id: i64,
    raw_title: &str,
    status: Option<String>,
) -> Result<TaskDto> {
    task_create_with(conn, project_id, raw_title, status, None, None, None, None, None)
}

#[allow(clippy::too_many_arguments)]
pub fn task_create_with(
    conn: &Connection,
    project_id: i64,
    raw_title: &str,
    status: Option<String>,
    due_date: Option<String>,
    due_time: Option<String>,
    is_priority: Option<bool>,
    checklist: Option<Vec<ChecklistItem>>,
    recurrence: Option<String>,
) -> Result<TaskDto> {
    ensure_project(conn, project_id)?;
    let title = raw_title.trim();
    if title.is_empty() {
        return Err(AppError::Invalid("task title is empty".into()));
    }
    let status = status.unwrap_or_else(|| "new".into());
    check_status(&status)?;
    let due_date = match due_date {
        Some(d) => {
            check_date(&d, "due_date")?;
            Some(d)
        }
        None => None,
    };
    let due_time = match due_time {
        Some(t) => {
            check_time(&t, "due_time")?;
            t
        }
        None => DEFAULT_DUE_TIME.to_owned(),
    };
    let is_priority = is_priority.unwrap_or(false);
    let recurrence = recurrence.unwrap_or_else(|| "none".into());
    check_recurrence(&recurrence)?;
    let checklist_json = match checklist {
        Some(items) => serde_json::to_string(&normalize_checklist(items)?)?,
        None => "[]".to_owned(),
    };
    let position: f64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks WHERE project_id = ?1 AND status = ?2",
        params![project_id, status],
        |r| r.get(0),
    )?;
    // A task created straight into 'progress' starts its timer right away
    // (same rule as a move into the column, SPEC 4.3).
    let started_at = if status == "progress" { now_ms(conn)? } else { 0 };
    conn.execute(
        "INSERT INTO tasks (project_id, title, status, due_date, due_time, position, created_at, closed_at,
                            is_priority, checklist, recurrence, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now','localtime'),
                 CASE WHEN ?3 = 'done' THEN datetime('now','localtime') ELSE NULL END,
                 ?7, ?8, ?9, CASE WHEN ?3 = 'progress' THEN ?10 ELSE NULL END)",
        params![project_id, title, status, due_date, due_time, position, is_priority, checklist_json, recurrence, started_at],
    )?;
    let id = conn.last_insert_rowid();
    fts_sync(conn, id)?;
    dto(conn, id)
}

pub fn task_update(conn: &Connection, id: i64, patch: TaskPatch) -> Result<TaskDto> {
    let (mut title, mut comment, mut due_date, mut due_time, mut remind_at, mut is_priority,
         mut checklist_json, mut recurrence) = conn
        .query_row(
            "SELECT title, comment, due_date, due_time, remind_at, is_priority, checklist, recurrence
             FROM tasks WHERE id = ?1",
            params![id],
            |r| Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, i64>(5)? != 0,
                r.get(6)?,
                r.get(7)?,
            )),
        )
        .map_err(|_| AppError::Invalid(format!("task {id} not found")))?;

    if let Some(new_title) = patch.title {
        let new_title = new_title.trim().to_owned();
        if new_title.is_empty() {
            return Err(AppError::Invalid("task title is empty".into()));
        }
        title = new_title;
    }
    if let Some(new_comment) = patch.comment {
        // An empty comment is stored as NULL.
        comment = new_comment
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty());
    }
    if let Some(new_due) = patch.due_date {
        if let Some(d) = &new_due {
            check_date(d, "due_date")?;
        }
        due_date = new_due;
    }
    // due_time is NOT NULL: an explicit null is indistinguishable from
    // absent and keeps the current value.
    if let Some(new_time) = patch.due_time {
        check_time(&new_time, "due_time")?;
        due_time = new_time;
    }
    if let Some(new_remind) = patch.remind_at {
        if let Some(r) = &new_remind {
            check_remind_at(r)?;
        }
        remind_at = new_remind;
    }
    if let Some(new_is_priority) = patch.is_priority {
        is_priority = new_is_priority;
    }
    if let Some(checklist) = patch.checklist {
        checklist_json = serde_json::to_string(&normalize_checklist(checklist)?)?;
    }
    if let Some(new_recurrence) = patch.recurrence {
        check_recurrence(&new_recurrence)?;
        recurrence = new_recurrence;
    }

    conn.execute(
        "UPDATE tasks SET title = ?1, comment = ?2, due_date = ?3, due_time = ?4, remind_at = ?5,
                        is_priority = ?6, checklist = ?7, recurrence = ?8
         WHERE id = ?9",
        params![title, comment, due_date, due_time, remind_at, is_priority, checklist_json, recurrence, id],
    )?;
    fts_sync(conn, id)?;
    dto(conn, id)
}

#[cfg(test)]
pub fn task_move(conn: &Connection, id: i64, status: &str, position: f64) -> Result<TaskDto> {
    Ok(task_move_capture(conn, id, status, position)?.0)
}

pub(crate) fn task_move_capture(
    conn: &Connection,
    id: i64,
    status: &str,
    position: f64,
) -> Result<(TaskDto, Option<i64>)> {
    check_status(status)?;
    let prev: Option<(String, Option<String>, String, String, String)> = conn
        .query_row(
            "SELECT status, due_date, recurrence, title, checklist FROM tasks WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (prev_status, due_date, recurrence, title, checklist_json) =
        prev.ok_or_else(|| AppError::Invalid(format!("task {id} not found")))?;
    conn.execute(
        "UPDATE tasks SET status = ?1, position = ?2,
         closed_at = CASE WHEN ?1 = 'done'
                          THEN COALESCE(closed_at, datetime('now','localtime'))
                          ELSE NULL END
         WHERE id = ?3",
        params![status, position, id],
    )?;
    apply_timer_transition(conn, id, status)?;
    // A recurring task completed in the kanban queues its next occurrence in
    // the 'new' column; the tasks-changed event (per project) covers both.
    let spawned = if status == "done" && prev_status != "done" && recurrence != "none" {
        spawn_next_occurrence(conn, id, &title, due_date.as_deref(), &recurrence, &checklist_json)?
    } else {
        None
    };
    Ok((dto(conn, id)?, spawned))
}

// Copies title/comment/is_priority/recurrence and the checklist (done
// reset); due date advances daily/weekly/monthly from the current one (today
// when unset). Monthly clamps to the end of the target month (Jan 31 +1
// month -> Feb 28), which plain date(x,'+1 month') would overflow into March.
// Guard: an identical next occurrence (same project, title and computed due
// date) is never queued twice, so repeated done moves stay safe.
fn spawn_next_occurrence(
    conn: &Connection,
    src_id: i64,
    title: &str,
    due_date: Option<&str>,
    recurrence: &str,
    checklist_json: &str,
) -> Result<Option<i64>> {
    let modifier = match recurrence {
        "daily" => "+1 day",
        "weekly" => "+7 days",
        "monthly" => "+1 month",
        _ => return Ok(None),
    };
    let base = match due_date {
        Some(d) => d.to_owned(),
        None => today(conn)?,
    };
    let next_due: String = if recurrence == "monthly" {
        // date(x,'start of month','+2 months','-1 day') is the last day of
        // the next month; keep the day of month when it fits, clamp when not.
        conn.query_row(
            "SELECT CASE
               WHEN CAST(strftime('%d', ?1) AS INTEGER)
                    <= CAST(strftime('%d', date(?1, 'start of month', '+2 months', '-1 day')) AS INTEGER)
               THEN date(?1, '+1 month')
               ELSE date(?1, 'start of month', '+2 months', '-1 day')
             END",
            params![base],
            |r| r.get(0),
        )?
    } else {
        conn.query_row(&format!("SELECT date(?1, '{modifier}')"), params![base], |r| r.get(0))?
    };
    let duplicate: i64 = conn.query_row(
        "SELECT count(*) FROM tasks
         WHERE project_id = (SELECT project_id FROM tasks WHERE id = ?1)
           AND title = ?2 AND due_date = ?3",
        params![src_id, title, next_due],
        |r| r.get(0),
    )?;
    if duplicate > 0 {
        return Ok(None);
    }
    let mut items: Vec<ChecklistItem> = serde_json::from_str(checklist_json).unwrap_or_default();
    for item in &mut items {
        item.done = false;
    }
    let checklist = serde_json::to_string(&items)?;
    let (project_id, comment, due_time, is_priority): (i64, Option<String>, String, i64) = conn.query_row(
        "SELECT project_id, comment, due_time, is_priority FROM tasks WHERE id = ?1",
        params![src_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let position: f64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks WHERE project_id = ?1 AND status = 'new'",
        params![project_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO tasks (project_id, title, status, comment, due_date, due_time, remind_at,
                            position, created_at, closed_at, is_priority, checklist, recurrence)
         VALUES (?1, ?2, 'new', ?3, ?4, ?5, NULL, ?6, datetime('now','localtime'), NULL, ?7, ?8, ?9)",
        params![project_id, title, comment, next_due, due_time, position, is_priority, checklist, recurrence],
    )?;
    let spawned_id = conn.last_insert_rowid();
    fts_sync(conn, spawned_id)?;
    Ok(Some(spawned_id))
}

// Full tasks row as JSON, snapshotted into the trash table before delete
// (kind='task', no file on disk); re-inserted by task_restore_from_trash.
// The serde defaults keep snapshots written before v25 restorable.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskSnapshot {
    pub(crate) id: i64,
    pub(crate) project_id: i64,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) comment: Option<String>,
    pub(crate) due_date: Option<String>,
    #[serde(default = "default_due_time")]
    pub(crate) due_time: String,
    pub(crate) remind_at: Option<String>,
    pub(crate) tags: String,
    pub(crate) position: f64,
    pub(crate) created_at: String,
    pub(crate) closed_at: Option<String>,
    #[serde(default = "default_priority")]
    pub(crate) priority: String,
    #[serde(default = "default_empty_json")]
    pub(crate) checklist: String,
    #[serde(default = "default_recurrence")]
    pub(crate) recurrence: String,
    #[serde(default)]
    pub(crate) is_priority: bool,
    // Timer state rides along so undo/trash restore puts the task back with
    // exactly the spent time and active-start stamp it had at snapshot time.
    // The serde defaults keep snapshots written before v33 restorable.
    #[serde(default)]
    pub(crate) spent_ms: i64,
    #[serde(default)]
    pub(crate) started_at: Option<i64>,
}

fn default_priority() -> String {
    "normal".to_owned()
}

fn default_empty_json() -> String {
    "[]".to_owned()
}

fn default_recurrence() -> String {
    "none".to_owned()
}

fn default_due_time() -> String {
    DEFAULT_DUE_TIME.to_owned()
}

pub(crate) fn task_snapshot(conn: &Connection, id: i64) -> Result<TaskSnapshot> {
    conn.query_row(
        "SELECT id, project_id, title, status, comment, due_date, due_time, remind_at,
                tags, position, created_at, closed_at, priority, checklist, recurrence, is_priority,
                spent_ms, started_at
         FROM tasks WHERE id = ?1",
        params![id],
        |r| {
            Ok(TaskSnapshot {
                id: r.get(0)?,
                project_id: r.get(1)?,
                title: r.get(2)?,
                status: r.get(3)?,
                comment: r.get(4)?,
                due_date: r.get(5)?,
                due_time: r.get(6)?,
                remind_at: r.get(7)?,
                tags: r.get(8)?,
                position: r.get(9)?,
                created_at: r.get(10)?,
                closed_at: r.get(11)?,
                priority: r.get(12)?,
                checklist: r.get(13)?,
                recurrence: r.get(14)?,
                is_priority: r.get::<_, i64>(15)? != 0,
                spent_ms: r.get(16)?,
                started_at: r.get(17)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("task {id} not found")))
}

// Returns the owning project id so the caller can re-export it. The row is
// snapshotted into the trash table first, so a deleted task stays restorable.
#[cfg(test)]
pub fn task_delete(conn: &Connection, id: i64) -> Result<i64> {
    Ok(task_delete_with_trash(conn, id)?.0)
}

pub(crate) fn task_delete_with_trash(conn: &Connection, id: i64) -> Result<(i64, i64)> {
    let snapshot = task_snapshot(conn, id)?;
    let project_id = snapshot.project_id;
    let trash_id = crate::reliability::trash_put_meta(
        conn,
        "task",
        "task",
        &format!("task:{id}"),
        &snapshot.title,
        &serde_json::to_string(&snapshot)?,
    )?;
    conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
    fts_sync(conn, id)?;
    Ok((project_id, trash_id))
}

pub(crate) fn task_delete_row(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
    fts_sync(conn, id)?;
    Ok(())
}

const RESTORE_COLUMNS: &str = "project_id, title, status, comment, due_date, due_time, remind_at,
                                 tags, position, created_at, closed_at, priority, checklist,
                                 recurrence, is_priority, spent_ms, started_at";

fn restore_params(snapshot: &TaskSnapshot) -> Vec<&dyn rusqlite::ToSql> {
    vec![
        &snapshot.project_id,
        &snapshot.title,
        &snapshot.status,
        &snapshot.comment,
        &snapshot.due_date,
        &snapshot.due_time,
        &snapshot.remind_at,
        &snapshot.tags,
        &snapshot.position,
        &snapshot.created_at,
        &snapshot.closed_at,
        &snapshot.priority,
        &snapshot.checklist,
        &snapshot.recurrence,
        &snapshot.is_priority,
        &snapshot.spent_ms,
        &snapshot.started_at,
    ]
}

fn insert_task_row(conn: &Connection, snapshot: &TaskSnapshot, with_id: bool) -> Result<()> {
    let mut params = restore_params(snapshot);
    let columns = if with_id {
        params.insert(0, &snapshot.id);
        "id, "
    } else {
        ""
    };
    let placeholders = vec!["?"; params.len()].join(", ");
    conn.execute(
        &format!(
            "INSERT INTO tasks ({columns}{RESTORE_COLUMNS}) VALUES ({placeholders})"
        ),
        params.as_slice(),
    )?;
    Ok(())
}

pub(crate) fn task_restore_snapshot(conn: &Connection, snapshot: &TaskSnapshot) -> Result<()> {
    let updated = conn.execute(
        "UPDATE tasks SET project_id = ?1, title = ?2, status = ?3, comment = ?4,
                due_date = ?5, due_time = ?6, remind_at = ?7, tags = ?8, position = ?9,
                created_at = ?10, closed_at = ?11, priority = ?12, checklist = ?13,
                recurrence = ?14, is_priority = ?15, spent_ms = ?16, started_at = ?17
         WHERE id = ?18",
        params![
            snapshot.project_id,
            snapshot.title,
            snapshot.status,
            snapshot.comment,
            snapshot.due_date,
            snapshot.due_time,
            snapshot.remind_at,
            snapshot.tags,
            snapshot.position,
            snapshot.created_at,
            snapshot.closed_at,
            snapshot.priority,
            snapshot.checklist,
            snapshot.recurrence,
            snapshot.is_priority,
            snapshot.spent_ms,
            snapshot.started_at,
            snapshot.id
        ],
    )?;
    if updated == 0 {
        insert_task_row(conn, snapshot, true)?;
    }
    fts_sync(conn, snapshot.id)?;
    Ok(())
}

// Re-inserts a trashed task snapshot (reliability::trash_restore calls this):
// the original id is reused while the slot is free, otherwise SQLite picks
// the next one. Returns the owning project id for the tasks-changed event.
pub(crate) fn task_restore_from_trash(conn: &Connection, row: &crate::reliability::TrashDto) -> Result<i64> {
    let meta = row.meta.as_deref().ok_or_else(|| {
        AppError::Invalid(format!("trash entry {} has no task snapshot", row.id))
    })?;
    let snapshot: TaskSnapshot = serde_json::from_str(meta)?;
    let taken: i64 = conn.query_row(
        "SELECT count(*) FROM tasks WHERE id = ?1",
        params![snapshot.id],
        |r| r.get(0),
    )?;
    if taken == 0 {
        task_restore_snapshot(conn, &snapshot)?;
    } else {
        insert_task_row(conn, &snapshot, false)?;
        fts_sync(conn, conn.last_insert_rowid())?;
    }
    Ok(snapshot.project_id)
}

// tasks_fts mirrors title/comment keyed by task id (SPEC 4.4); search joins
// tasks for scoping and status, so closed tasks stay findable. The contentless
// table rejects UPDATE, so re-sync is delete-then-insert; the delete also
// clears a rowid reused by a newer task. task_move changes only status (kept
// in tasks, not in the mirror), so it needs no sync. The legacy tags column of
// the mirror is written empty: tags are no longer part of the task model.
fn fts_sync(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM tasks_fts WHERE rowid = ?1", params![id])?;
    let row = conn
        .query_row(
            "SELECT title, comment FROM tasks WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((title, comment)) = row {
        conn.execute(
            "INSERT INTO tasks_fts (rowid, title, comment, tags) VALUES (?1, ?2, ?3, '')",
            params![id, title, comment],
        )?;
    }
    Ok(())
}

fn dto(conn: &Connection, id: i64) -> Result<TaskDto> {
    let now = now(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT project_id, title, status, comment, due_date, due_time, remind_at, position, created_at, closed_at,
                is_priority, checklist, recurrence, spent_ms, started_at
         FROM tasks WHERE id = ?1 {ORDER_SQL}"
    ))?;
    let mut rows = stmt.query(params![id])?;
    let row = rows
        .next()?
        .ok_or_else(|| AppError::Invalid(format!("task {id} not found")))?;
    let project_id: i64 = row.get(0)?;
    let status: String = row.get(2)?;
    let due_date: Option<String> = row.get(4)?;
    let due_time: String = row.get(5)?;
    let overdue = is_overdue(&status, due_date.as_deref(), &due_time, &now);
    Ok(TaskDto {
        id,
        project_id,
        title: row.get(1)?,
        overdue,
        position: row.get(7)?,
        created_at: row.get(8)?,
        closed_at: row.get(9)?,
        comment: row.get(3)?,
        due_date,
        due_time,
        remind_at: row.get(6)?,
        status,
        is_priority: row.get::<_, i64>(10)? != 0,
        checklist: parse_checklist(&row.get::<_, String>(11)?)?,
        recurrence: row.get(12)?,
        spent_ms: row.get(13)?,
        started_at: row.get(14)?,
    })
}

fn today(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT date('now','localtime')", [], |r| r.get(0))?)
}

fn now(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT datetime('now','localtime')", [], |r| r.get(0))?)
}

// Wall-clock epoch ms (UTC): the timer never parses date/time strings and
// keeps running while the app is closed (SPEC 4.3).
fn now_ms(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER) * 1000", [], |r| r.get(0))?)
}

// The single place where a status change touches the task timer; every
// path (board move, agent move, create-in-progress in task_create_with)
// lands here, so none can double-count. SQLite evaluates every SET
// right-hand side against the old row, so spent_ms banks the elapsed
// wall-clock time from the old started_at before the marker is cleared.
// Entering 'progress' keeps an existing started_at (a same-status move
// adds nothing); leaving it clears the marker; other transitions are
// no-ops.
fn apply_timer_transition(conn: &Connection, id: i64, new_status: &str) -> Result<()> {
    let now = now_ms(conn)?;
    conn.execute(
        "UPDATE tasks SET
           spent_ms = spent_ms + CASE WHEN ?2 <> 'progress' AND started_at IS NOT NULL
                                      THEN ?3 - started_at ELSE 0 END,
           started_at = CASE WHEN ?2 = 'progress' THEN COALESCE(started_at, ?3) ELSE NULL END
         WHERE id = ?1",
        params![id, new_status, now],
    )?;
    Ok(())
}

// Overdue by deadline date + time (SPEC 4.3): "YYYY-MM-DD HH:MM:00" and the
// local "YYYY-MM-DD HH:MM:SS" wall clock compare correctly as strings, so a
// task is overdue once its due date+time is strictly in the past.
fn is_overdue(status: &str, due_date: Option<&str>, due_time: &str, now: &str) -> bool {
    status != "done"
        && due_date.is_some_and(|d| {
            let due = format!("{d} {due_time}:00");
            due.as_str() < now
        })
}

fn parse_checklist(raw: &str) -> Result<Vec<ChecklistItem>> {
    Ok(serde_json::from_str(raw).unwrap_or_default())
}

fn ensure_project(conn: &Connection, id: i64) -> Result<()> {
    let exists: i64 =
        conn.query_row("SELECT count(*) FROM projects WHERE id = ?1", params![id], |r| r.get(0))?;
    if exists == 0 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    Ok(())
}

fn check_status(status: &str) -> Result<()> {
    if STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown task status \"{status}\", expected one of {STATUSES:?}"
        )))
    }
}

fn check_recurrence(recurrence: &str) -> Result<()> {
    if RECURRENCES.contains(&recurrence) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown task recurrence \"{recurrence}\", expected one of {RECURRENCES:?}"
        )))
    }
}

// Blank item texts are dropped (like empty tags); the list is capped so a
// task stays a kanban card, not a document.
fn normalize_checklist(items: Vec<ChecklistItem>) -> Result<Vec<ChecklistItem>> {
    let cleaned: Vec<ChecklistItem> = items
        .into_iter()
        .map(|item| ChecklistItem {
            text: item.text.trim().to_owned(),
            done: item.done,
        })
        .filter(|item| !item.text.is_empty())
        .collect();
    if cleaned.len() > MAX_CHECKLIST_ITEMS {
        return Err(AppError::Invalid(format!(
            "checklist is limited to {MAX_CHECKLIST_ITEMS} items, got {}",
            cleaned.len()
        )));
    }
    Ok(cleaned)
}

fn check_date(s: &str, label: &str) -> Result<()> {
    let parts: Vec<&str> = s.split('-').collect();
    let valid = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
        && parts[1].parse::<u8>().is_ok_and(|m| (1..=12).contains(&m))
        && parts[2].parse::<u8>().is_ok_and(|d| (1..=31).contains(&d));
    if valid {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "{label} must be YYYY-MM-DD, got \"{s}\""
        )))
    }
}

// HH:MM wall-clock time, same shape the remind_at time part is checked with.
fn check_time(s: &str, label: &str) -> Result<()> {
    let parts: Vec<&str> = s.split(':').collect();
    let valid = parts.len() == 2
        && parts[0].len() == 2
        && parts[1].len() == 2
        && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
        && parts[0].parse::<u8>().is_ok_and(|h| h < 24)
        && parts[1].parse::<u8>().is_ok_and(|m| m < 60);
    if valid {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "{label} must be HH:MM, got \"{s}\""
        )))
    }
}

fn check_remind_at(s: &str) -> Result<()> {
    match s.split_once('T') {
        Some((date, time)) => {
            check_date(date, "remind_at date")?;
            let parts: Vec<&str> = time.split(':').collect();
            let valid = parts.len() == 2
                && parts[0].len() == 2
                && parts[1].len() == 2
                && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit()))
                && parts[0].parse::<u8>().is_ok_and(|h| h < 24)
                && parts[1].parse::<u8>().is_ok_and(|m| m < 60);
            if valid {
                Ok(())
            } else {
                Err(AppError::Invalid(format!(
                    "remind_at time must be HH:MM, got \"{time}\""
                )))
            }
        }
        None => Err(AppError::Invalid(format!(
            "remind_at must be YYYY-MM-DDTHH:MM, got \"{s}\""
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::reliability;
    use std::fs;
    use std::path::PathBuf;

    fn setup(tag: &str) -> (PathBuf, Connection, i64) {
        let wd = temp_test_dir(tag);
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        let project = projects::create_project(&conn, &wd, "p1").unwrap();
        (wd, conn, project.id)
    }

    #[test]
    fn crud_round_trip() {
        let (wd, conn, pid) = setup("tasks-crud");
        let t1 = task_create(&conn, pid, "  first  ", None).unwrap();
        assert_eq!(t1.title, "first");
        assert_eq!(t1.status, "new");
        assert_eq!(t1.position, 1.0);
        assert!(t1.created_at.len() >= 10);
        let t2 = task_create(&conn, pid, "second", Some("progress".into())).unwrap();
        assert_eq!(t2.status, "progress");
        let t3 = task_create(&conn, pid, "third", None).unwrap();
        assert_eq!(t3.position, 2.0); // appended at end of the 'new' column

        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.iter().map(|t| t.id).collect::<Vec<_>>(), vec![t1.id, t3.id, t2.id]);

        let updated = task_update(
            &conn,
            t1.id,
            TaskPatch {
                title: Some("renamed".into()),
                comment: Some(Some("note".into())),
                due_date: Some(Some("2099-01-02".into())),
                remind_at: Some(Some("2099-01-02T10:30".into())),
                is_priority: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(updated.comment.as_deref(), Some("note"));
        assert_eq!(updated.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(updated.remind_at.as_deref(), Some("2099-01-02T10:30"));
        assert!(updated.is_priority);

        let moved = task_move(&conn, t1.id, "progress", 9.5).unwrap();
        assert_eq!(moved.status, "progress");
        assert_eq!(moved.position, 9.5);
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.iter().map(|t| t.id).collect::<Vec<_>>(), vec![t3.id, t2.id, t1.id]);

        assert_eq!(task_delete(&conn, t1.id).unwrap(), pid);
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 2);
        assert!(matches!(task_delete(&conn, t1.id), Err(AppError::Invalid(_))));
        assert!(matches!(
            task_create(&conn, 999, "x", None),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_leaves_a_task_trash_row_with_the_full_snapshot() {
        let (wd, conn, pid) = setup("tasks-trash-shape");
        let t = task_create_with(
            &conn,
            pid,
            "отчёт",
            Some("progress".into()),
            Some("2099-01-02".into()),
            None,
            Some(true),
            Some(vec![ChecklistItem { text: "send".into(), done: false }]),
            Some("weekly".into()),
        )
        .unwrap();
        let (project_id, trash_id) = task_delete_with_trash(&conn, t.id).unwrap();
        assert_eq!(project_id, pid);
        let row: (String, String, String, String, String, String, i64) = conn
            .query_row(
                "SELECT scope, kind, original_path, stored_path, name, meta, id
                 FROM trash WHERE id = ?1",
                params![trash_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        assert_eq!(row.0, "task");
        assert_eq!(row.1, "task");
        assert_eq!(row.2, format!("task:{}", t.id));
        assert_eq!(row.3, "", "a task trash row keeps no file on disk");
        assert_eq!(row.4, "отчёт");
        assert_eq!(row.6, trash_id);
        let meta: serde_json::Value = serde_json::from_str(&row.5).unwrap();
        assert_eq!(meta["id"], t.id);
        assert_eq!(meta["projectId"], pid);
        assert_eq!(meta["status"], "progress");
        assert_eq!(meta["dueDate"], "2099-01-02");
        assert_eq!(meta["dueTime"], "07:00");
        assert_eq!(meta["isPriority"], true);
        assert_eq!(meta["recurrence"], "weekly");
        assert_eq!(meta["checklist"], "[{\"text\":\"send\",\"done\":false}]");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_of_a_missing_task_errors_without_touching_rows_or_trash() {
        let (wd, conn, pid) = setup("tasks-delete-rollback");
        let t = task_create(&conn, pid, "keep", None).unwrap();
        assert!(matches!(task_delete_with_trash(&conn, 9999), Err(AppError::Invalid(_))));
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 1);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM trash", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "a failed delete must not leave a trash row"
        );
        assert_eq!(task_snapshot(&conn, t.id).unwrap().title, "keep");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trash_restore_reinserts_the_same_id_and_reindexes_fts() {
        let (wd, conn, pid) = setup("tasks-trash-restore");
        let t = task_create(&conn, pid, "восстановить", None).unwrap();
        let (_, trash_id) = task_delete_with_trash(&conn, t.id).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM tasks_fts WHERE tasks_fts MATCH ?1",
                params!["восстановить"],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "delete must clear the fts mirror"
        );
        let row = reliability::TrashDto {
            id: trash_id,
            scope: "task".into(),
            kind: "task".into(),
            original_path: format!("task:{}", t.id),
            stored_path: String::new(),
            name: "восстановить".into(),
            deleted_at: String::new(),
            meta: conn
                .query_row("SELECT meta FROM trash WHERE id = ?1", params![trash_id], |r| r.get(0))
                .unwrap(),
            project_id: None,
        };
        let restored_pid = task_restore_from_trash(&conn, &row).unwrap();
        assert_eq!(restored_pid, pid);
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, t.id, "restore must reuse the original id");
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM tasks_fts WHERE tasks_fts MATCH ?1",
                params!["восстановить"],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "restore must reindex the fts mirror"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn dto_json_carries_is_priority_and_drops_tags_and_priority() {
        let (wd, conn, pid) = setup("tasks-dto-json");
        let t = task_create_with(
            &conn,
            pid,
            "t",
            None,
            Some("2099-01-02".into()),
            None,
            Some(true),
            None,
            None,
        )
        .unwrap();
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["isPriority"], true);
        assert_eq!(json["dueDate"], "2099-01-02");
        assert!(json.get("tags").is_none(), "tags must be gone from the DTO");
        assert!(json.get("priority").is_none(), "priority must be gone from the DTO");
        // Stale priority/tags keys an old caller may still send are ignored.
        let patch: TaskPatch = serde_json::from_str(r#"{"priority": "urgent", "tags": ["x"]}"#).unwrap();
        let kept = task_update(&conn, t.id, patch).unwrap();
        assert!(kept.is_priority, "stale keys must not flip the flag");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn fts_mirror_tracks_task_mutations() {
        let (wd, conn, pid) = setup("tasks-fts");
        let fts_count = |q: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM tasks_fts WHERE tasks_fts MATCH ?1",
                params![q],
                |r| r.get(0),
            )
            .unwrap()
        };

        let t = task_create(&conn, pid, "старое слово", None).unwrap();
        assert_eq!(fts_count("старое"), 1, "create must index the task");

        task_update(
            &conn,
            t.id,
            TaskPatch {
                title: Some("новое слово".into()),
                comment: Some(Some("комментарий задача".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fts_count("старое"), 0, "old text must be gone after update");
        assert_eq!(fts_count("новое"), 1);
        assert_eq!(fts_count("комментарий"), 1);

        task_move(&conn, t.id, "done", 1.0).unwrap();
        assert_eq!(fts_count("новое"), 1, "closed tasks stay in the mirror");

        task_delete(&conn, t.id).unwrap();
        assert_eq!(fts_count("новое"), 0, "delete must remove the mirror row");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn move_sets_and_clears_closed_at() {
        let (wd, conn, pid) = setup("tasks-closed-at");
        let t = task_create(&conn, pid, "t", None).unwrap();
        assert!(t.closed_at.is_none());
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert!(done.closed_at.is_some());
        let back = task_move(&conn, t.id, "progress", 1.0).unwrap();
        assert_eq!(back.closed_at, None);
        let again = task_move(&conn, t.id, "done", 2.0).unwrap();
        assert!(again.closed_at.is_some());
        let born_done = task_create(&conn, pid, "born", Some("done".into())).unwrap();
        assert!(born_done.closed_at.is_some());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_start_stop_resume_accumulates() {
        let (wd, conn, pid) = setup("tasks-timer-accumulate");
        let t = task_create(&conn, pid, "t", None).unwrap();
        assert_eq!(t.spent_ms, 0);
        assert_eq!(t.started_at, None, "a 'new' task has no active start");

        // Entering 'progress' starts the clock.
        let started = task_move(&conn, t.id, "progress", 1.0).unwrap();
        assert!(started.started_at.is_some());
        assert_eq!(started.spent_ms, 0);

        // Back to 'new' stops it and banks the elapsed time.
        let stopped = task_move(&conn, t.id, "new", 1.0).unwrap();
        assert_eq!(stopped.started_at, None);
        assert!(stopped.spent_ms >= 0);

        // Returning to 'progress' resumes: the stamp is fresh, the banked
        // time is kept (not reset, not double-counted).
        let resumed = task_move(&conn, t.id, "progress", 1.0).unwrap();
        assert!(resumed.started_at.is_some());
        assert_eq!(resumed.spent_ms, stopped.spent_ms);

        // 'done' stops it too; the total survives.
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert_eq!(done.started_at, None);
        assert!(done.spent_ms >= stopped.spent_ms);

        // A task created straight into 'progress' starts counting at once.
        let born_running = task_create(&conn, pid, "born", Some("progress".into())).unwrap();
        assert!(born_running.started_at.is_some());
        assert_eq!(born_running.spent_ms, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_wall_clock_accumulates_across_a_simulated_gap() {
        let (wd, conn, pid) = setup("tasks-timer-wallclock");
        let t = task_create(&conn, pid, "t", None).unwrap();
        task_move(&conn, t.id, "progress", 1.0).unwrap();
        // Simulate the app having been closed for two hours: rewind the
        // start stamp; the next stop must bank the whole wall-clock span.
        conn.execute(
            "UPDATE tasks SET started_at = started_at - 7200000 WHERE id = ?1",
            params![t.id],
        )
        .unwrap();
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert!(
            done.spent_ms >= 7_200_000 && done.spent_ms < 7_205_000,
            "two simulated hours must land in spent_ms, got {}",
            done.spent_ms
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_no_double_count_on_same_status_moves() {
        let (wd, conn, pid) = setup("tasks-timer-no-double");
        let t = task_create(&conn, pid, "t", None).unwrap();
        let started = task_move(&conn, t.id, "progress", 1.0).unwrap();
        let stamp = started.started_at.unwrap();

        // Reordering inside the column (same status) must not restart or
        // bank anything.
        let reordered = task_move(&conn, t.id, "progress", 9.0).unwrap();
        assert_eq!(reordered.started_at, Some(stamp), "same-status move must keep the stamp");
        assert_eq!(reordered.spent_ms, 0);

        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        let total = done.spent_ms;
        assert!(total >= 0);
        // Repeating the done move must not bank anything a second time.
        let again = task_move(&conn, t.id, "done", 2.0).unwrap();
        assert_eq!(again.spent_ms, total);
        assert_eq!(again.started_at, None);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_state_is_carried_through_dto_json_and_trash_snapshot() {
        let (wd, conn, pid) = setup("tasks-timer-dto");
        let t = task_create(&conn, pid, "t", Some("progress".into())).unwrap();
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["spentMs"], 0);
        assert!(json["startedAt"].is_i64(), "the DTO must expose the running start");

        let (project_id, trash_id) = task_delete_with_trash(&conn, t.id).unwrap();
        assert_eq!(project_id, pid);
        let raw: String = conn
            .query_row("SELECT meta FROM trash WHERE id = ?1", params![trash_id], |r| r.get(0))
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(meta["spentMs"], 0);
        assert_eq!(meta["startedAt"], json["startedAt"], "the trash snapshot must keep the timer");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn overdue_is_derived_in_rust() {
        let (wd, conn, pid) = setup("tasks-overdue");
        let yesterday: String = conn
            .query_row("SELECT date('now','localtime','-1 day')", [], |r| r.get(0))
            .unwrap();
        let tomorrow: String = conn
            .query_row("SELECT date('now','localtime','+1 day')", [], |r| r.get(0))
            .unwrap();

        let t = task_create(&conn, pid, "late", None).unwrap();
        let t = task_update(&conn, t.id, TaskPatch { due_date: Some(Some(yesterday.clone())), ..Default::default() }).unwrap();
        assert!(t.overdue);
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert!(!done.overdue, "done tasks are never overdue");
        let back = task_move(&conn, t.id, "new", 1.0).unwrap();
        assert!(back.overdue);

        let future = task_create(&conn, pid, "future", None).unwrap();
        let future = task_update(&conn, future.id, TaskPatch { due_date: Some(Some(tomorrow)), ..Default::default() }).unwrap();
        assert!(!future.overdue);
        assert!(!tasks_list(&conn, pid).unwrap().iter().any(|x| x.id == future.id && x.overdue));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn overdue_boundary_compares_date_plus_time() {
        // Pure boundary check around the 07:00 default: at 07:00:00 sharp the
        // task is not yet overdue, one minute later it is.
        let now = "2099-01-02 07:00:00";
        assert!(is_overdue("new", Some("2099-01-02"), "06:59", now));
        assert!(!is_overdue("new", Some("2099-01-02"), "07:00", now));
        assert!(!is_overdue("new", Some("2099-01-02"), "07:01", now));
        assert!(!is_overdue("done", Some("2099-01-02"), "06:59", now));
        assert!(!is_overdue("new", None, "06:59", now));
        assert!(is_overdue("progress", Some("2099-01-01"), "23:59", now));
        assert!(!is_overdue("progress", Some("2099-01-03"), "00:00", now));

        // Against the live DB clock: today at one minute ago is overdue, at
        // one minute ahead is not, whatever time of day the test runs at.
        let (wd, conn, pid) = setup("tasks-overdue-time");
        let (past_date, past_time) = conn
            .query_row(
                "SELECT date('now','localtime','-1 minute'), strftime('%H:%M','now','localtime','-1 minute')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (future_date, future_time) = conn
            .query_row(
                "SELECT date('now','localtime','+1 minute'), strftime('%H:%M','now','localtime','+1 minute')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let late = task_create_with(&conn, pid, "minute-ago", None, Some(past_date), Some(past_time), None, None, None).unwrap();
        assert!(late.overdue, "due one minute ago must be overdue");
        let ahead = task_create_with(&conn, pid, "minute-ahead", None, Some(future_date), Some(future_time), None, None, None).unwrap();
        assert!(!ahead.overdue, "due one minute ahead must not be overdue");
        let listed = tasks_list(&conn, pid).unwrap();
        assert!(listed.iter().any(|t| t.id == late.id && t.overdue));
        assert!(listed.iter().any(|t| t.id == ahead.id && !t.overdue));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn due_time_defaults_to_0700_and_persists() {
        let (wd, conn, pid) = setup("tasks-due-time");
        // Absent time on create -> 07:00, even with a due date.
        let t = task_create_with(&conn, pid, "plain", None, Some("2099-01-02".into()), None, None, None, None).unwrap();
        assert_eq!(t.due_time, "07:00");
        assert_eq!(serde_json::to_value(&t).unwrap()["dueTime"], "07:00");
        let explicit = task_create_with(&conn, pid, "timed", None, Some("2099-01-02".into()), Some("23:45".into()), None, None, None).unwrap();
        assert_eq!(explicit.due_time, "23:45");

        // Update sets it; absent or explicit null keeps it (NOT NULL column).
        let patched: TaskPatch = serde_json::from_str(r#"{"dueTime": "10:30"}"#).unwrap();
        let patched = task_update(&conn, t.id, patched).unwrap();
        assert_eq!(patched.due_time, "10:30");
        let kept: TaskPatch = serde_json::from_str(r#"{"dueTime": null, "title": "renamed"}"#).unwrap();
        let kept = task_update(&conn, t.id, kept).unwrap();
        assert_eq!(kept.due_time, "10:30");
        let kept = task_update(&conn, t.id, TaskPatch::default()).unwrap();
        assert_eq!(kept.due_time, "10:30");

        // Malformed times are rejected and change nothing.
        for bad in ["7:00", "07:60", "24:00", "0700", "07:0a", "07:00:00", ""] {
            assert!(
                matches!(
                    task_update(&conn, t.id, TaskPatch { due_time: Some(bad.into()), ..Default::default() }),
                    Err(AppError::Invalid(_))
                ),
                "accepted due_time {bad:?}"
            );
            assert!(
                matches!(
                    task_create_with(&conn, pid, "x", None, None, Some(bad.into()), None, None, None),
                    Err(AppError::Invalid(_))
                ),
                "accepted due_time {bad:?} on create"
            );
        }
        assert_eq!(dto(&conn, t.id).unwrap().due_time, "10:30");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn empty_comment_is_stored_as_null() {
        let (wd, conn, pid) = setup("tasks-comment-null");
        let t = task_create(&conn, pid, "t", None).unwrap();
        let t = task_update(&conn, t.id, TaskPatch { comment: Some(Some("  ".into())), ..Default::default() }).unwrap();
        assert_eq!(t.comment, None);
        let stored: Option<String> = conn
            .query_row("SELECT comment FROM tasks WHERE id = ?1", params![t.id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn patch_absent_leaves_value_explicit_null_clears() {
        let (wd, conn, pid) = setup("tasks-patch-null");
        let t = task_create(&conn, pid, "t", None).unwrap();
        let t = task_update(
            &conn,
            t.id,
            TaskPatch {
                comment: Some(Some("note".into())),
                due_date: Some(Some("2099-01-02".into())),
                remind_at: Some(Some("2099-01-02T08:00".into())),
                is_priority: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

        let empty: TaskPatch = serde_json::from_str("{}").unwrap();
        let kept = task_update(&conn, t.id, empty).unwrap();
        assert_eq!(kept.comment.as_deref(), Some("note"));
        assert_eq!(kept.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(kept.remind_at.as_deref(), Some("2099-01-02T08:00"));
        assert!(kept.is_priority);

        let cleared: TaskPatch =
            serde_json::from_str(r#"{"comment": null, "dueDate": null, "remindAt": null}"#).unwrap();
        let cleared = task_update(&conn, t.id, cleared).unwrap();
        assert_eq!(cleared.comment, None);
        assert_eq!(cleared.due_date, None);
        assert_eq!(cleared.remind_at, None);
        assert!(cleared.is_priority);

        let set_again: TaskPatch = serde_json::from_str(r#"{"comment": "back"}"#).unwrap();
        assert_eq!(task_update(&conn, t.id, set_again).unwrap().comment.as_deref(), Some("back"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn invalid_status_title_and_dates_are_rejected() {
        let (wd, conn, pid) = setup("tasks-invalid");
        let t = task_create(&conn, pid, "t", None).unwrap();
        assert!(matches!(task_create(&conn, pid, "  ", None), Err(AppError::Invalid(_))));
        assert!(matches!(
            task_create(&conn, pid, "t", Some("bogus".into())),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(task_move(&conn, t.id, "bogus", 1.0), Err(AppError::Invalid(_))));
        assert!(matches!(task_move(&conn, 999, "done", 1.0), Err(AppError::Invalid(_))));
        assert!(matches!(
            task_update(&conn, t.id, TaskPatch { title: Some(" ".into()), ..Default::default() }),
            Err(AppError::Invalid(_))
        ));
        for bad in ["2099-13-01", "2099-1-01", "20990101", "2099-01-32"] {
            assert!(
                matches!(
                    task_update(&conn, t.id, TaskPatch { due_date: Some(Some(bad.into())), ..Default::default() }),
                    Err(AppError::Invalid(_))
                ),
                "accepted due_date {bad:?}"
            );
        }
        for bad in ["2099-01-02 10:00", "2099-01-02T25:00", "2099-01-02T10:60", "10:00"] {
            assert!(
                matches!(
                    task_update(&conn, t.id, TaskPatch { remind_at: Some(Some(bad.into())), ..Default::default() }),
                    Err(AppError::Invalid(_))
                ),
                "accepted remind_at {bad:?}"
            );
        }
        // Failed updates must not change stored values.
        let fresh = tasks_list(&conn, pid).unwrap().remove(0);
        assert_eq!(fresh.title, "t");
        assert_eq!(fresh.due_date, None);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn is_priority_checklist_recurrence_round_trip() {
        let (wd, conn, pid) = setup("tasks-upgrade-fields");
        let t = task_create_with(
            &conn,
            pid,
            "report",
            None,
            Some("2099-01-02".into()),
            None,
            Some(true),
            Some(vec![
                ChecklistItem { text: " draft ".into(), done: true },
                ChecklistItem { text: "send".into(), done: false },
            ]),
            Some("weekly".into()),
        )
        .unwrap();
        assert!(t.is_priority);
        assert_eq!(t.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(t.recurrence, "weekly");
        assert_eq!(
            t.checklist,
            vec![
                ChecklistItem { text: "draft".into(), done: true },
                ChecklistItem { text: "send".into(), done: false },
            ]
        );
        assert!(tasks_list(&conn, pid).unwrap()[0].is_priority);

        // camelCase JSON patch: absent fields keep values, set fields replace.
        let patched: TaskPatch = serde_json::from_str(
            r#"{"isPriority": false, "checklist": [{"text": "only", "done": true}], "recurrence": "monthly"}"#,
        )
        .unwrap();
        let patched = task_update(&conn, t.id, patched).unwrap();
        assert!(!patched.is_priority);
        assert_eq!(patched.recurrence, "monthly");
        assert_eq!(patched.checklist, vec![ChecklistItem { text: "only".into(), done: true }]);

        // Defaults on plain create; null for NOT NULL columns keeps values.
        let plain = task_create(&conn, pid, "plain", None).unwrap();
        assert!(!plain.is_priority);
        assert_eq!(plain.due_date, None);
        assert_eq!(plain.recurrence, "none");
        assert!(plain.checklist.is_empty());
        let kept: TaskPatch =
            serde_json::from_str(r#"{"isPriority": null, "checklist": null, "recurrence": null}"#)
                .unwrap();
        let kept = task_update(&conn, t.id, kept).unwrap();
        assert!(!kept.is_priority);
        assert_eq!(kept.recurrence.as_str(), "monthly");
        assert_eq!(kept.checklist.len(), 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn recurrence_and_checklist_are_validated() {
        let (wd, conn, pid) = setup("tasks-upgrade-invalid");
        let t = task_create(&conn, pid, "t", None).unwrap();
        for bad in ["yearly", "hourly", ""] {
            assert!(
                matches!(
                    task_create_with(&conn, pid, "t", None, None, None, None, None, Some(bad.into())),
                    Err(AppError::Invalid(_))
                ),
                "accepted recurrence {bad:?}"
            );
            assert!(
                matches!(
                    task_update(&conn, t.id, TaskPatch { recurrence: Some(bad.into()), ..Default::default() }),
                    Err(AppError::Invalid(_))
                ),
                "accepted recurrence {bad:?} on update"
            );
        }
        // A bad due date on create is rejected before any insert.
        assert!(matches!(
            task_create_with(&conn, pid, "t", None, Some("2099-13-01".into()), None, None, None, None),
            Err(AppError::Invalid(_))
        ));
        let too_many = vec![ChecklistItem { text: "x".into(), done: false }; MAX_CHECKLIST_ITEMS + 1];
        assert!(matches!(
            task_update(&conn, t.id, TaskPatch { checklist: Some(too_many), ..Default::default() }),
            Err(AppError::Invalid(_))
        ));
        // Blank texts are dropped, so the cap counts real items only.
        let blanks = vec![ChecklistItem { text: "  ".into(), done: false }; MAX_CHECKLIST_ITEMS + 1];
        let ok = task_update(&conn, t.id, TaskPatch { checklist: Some(blanks), ..Default::default() }).unwrap();
        assert!(ok.checklist.is_empty());
        let at_cap = vec![ChecklistItem { text: "x".into(), done: false }; MAX_CHECKLIST_ITEMS];
        assert_eq!(
            task_update(&conn, t.id, TaskPatch { checklist: Some(at_cap), ..Default::default() })
                .unwrap()
                .checklist
                .len(),
            MAX_CHECKLIST_ITEMS
        );
        // Failed patches must not change stored values.
        let fresh = dto(&conn, t.id).unwrap();
        assert_eq!(fresh.recurrence.as_str(), "none");
        assert!(!fresh.is_priority);
        assert_eq!(fresh.checklist.len(), MAX_CHECKLIST_ITEMS);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn recurrence_done_move_queues_next_occurrence() {
        let (wd, conn, pid) = setup("tasks-recurrence-spawn");
        let t = task_create_with(
            &conn,
            pid,
            "standup",
            Some("progress".into()),
            None,
            Some("09:15".into()),
            Some(true),
            Some(vec![ChecklistItem { text: "prep".into(), done: true }]),
            Some("daily".into()),
        )
        .unwrap();
        let t = task_update(
            &conn,
            t.id,
            TaskPatch {
                due_date: Some(Some("2099-01-02".into())),
                comment: Some(Some("daily sync".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert_eq!(done.status, "done");

        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.len(), 2, "done move must queue the next occurrence");
        let next = list.iter().find(|x| x.id != t.id).unwrap();
        assert_eq!(next.status, "new");
        assert_eq!(next.title, "standup");
        assert_eq!(next.due_date.as_deref(), Some("2099-01-03"));
        assert_eq!(next.due_time, "09:15", "the due time must be copied");
        assert!(next.is_priority, "the priority flag must be copied");
        assert_eq!(next.recurrence, "daily");
        assert_eq!(next.comment.as_deref(), Some("daily sync"));
        assert_eq!(next.checklist, vec![ChecklistItem { text: "prep".into(), done: false }]);
        assert!(next.closed_at.is_none());
        assert!(next.remind_at.is_none());
        assert_eq!(next.spent_ms, 0, "the spawned copy starts with a zero timer");
        assert_eq!(next.started_at, None, "the spawned copy is not running");

        // weekly advances 7 days; monthly clamps month ends like SQLite date().
        let weekly = task_create_with(&conn, pid, "w", None, None, None, None, None, Some("weekly".into())).unwrap();
        let weekly = task_update(&conn, weekly.id, TaskPatch { due_date: Some(Some("2099-01-30".into())), ..Default::default() }).unwrap();
        task_move(&conn, weekly.id, "done", 1.0).unwrap();
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(
            list.iter().find(|x| x.id != weekly.id && x.title == "w").unwrap().due_date.as_deref(),
            Some("2099-02-06")
        );
        let monthly = task_create_with(&conn, pid, "m", None, None, None, None, None, Some("monthly".into())).unwrap();
        let monthly = task_update(&conn, monthly.id, TaskPatch { due_date: Some(Some("2099-01-31".into())), ..Default::default() }).unwrap();
        task_move(&conn, monthly.id, "done", 1.0).unwrap();
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(
            list.iter().find(|x| x.id != monthly.id && x.title == "m").unwrap().due_date.as_deref(),
            Some("2099-02-28"),
            "monthly must clamp to the end of the target month"
        );
        // A day that exists in the target month advances plainly.
        let monthly2 = task_create_with(&conn, pid, "m2", None, None, None, None, None, Some("monthly".into())).unwrap();
        let monthly2 = task_update(&conn, monthly2.id, TaskPatch { due_date: Some(Some("2099-01-15".into())), ..Default::default() }).unwrap();
        task_move(&conn, monthly2.id, "done", 1.0).unwrap();
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(
            list.iter().find(|x| x.id != monthly2.id && x.title == "m2").unwrap().due_date.as_deref(),
            Some("2099-02-15")
        );

        // No due date: the next occurrence is dated from today.
        let undated = task_create_with(&conn, pid, "u", None, None, None, None, None, Some("daily".into())).unwrap();
        task_move(&conn, undated.id, "done", 1.0).unwrap();
        let list = tasks_list(&conn, pid).unwrap();
        let next_u = list.iter().find(|x| x.id != undated.id && x.title == "u").unwrap();
        let expected: String = conn
            .query_row("SELECT date('now','localtime','+1 day')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(next_u.due_date.as_deref(), Some(expected.as_str()));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn recurrence_none_and_non_done_moves_do_not_spawn() {
        let (wd, conn, pid) = setup("tasks-recurrence-none");
        let t = task_create(&conn, pid, "plain", None).unwrap();
        task_move(&conn, t.id, "progress", 1.0).unwrap();
        task_move(&conn, t.id, "done", 1.0).unwrap();
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 1);

        let r = task_create_with(&conn, pid, "weekly", None, None, None, None, None, Some("weekly".into())).unwrap();
        task_move(&conn, r.id, "progress", 1.0).unwrap();
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 2, "progress move must not spawn");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn recurrence_no_duplicate_on_repeated_done_moves() {
        let (wd, conn, pid) = setup("tasks-recurrence-dedup");
        let t = task_create_with(&conn, pid, "daily", None, None, None, None, None, Some("daily".into())).unwrap();
        let t = task_update(&conn, t.id, TaskPatch { due_date: Some(Some("2099-01-02".into())), ..Default::default() }).unwrap();
        task_move(&conn, t.id, "done", 1.0).unwrap();
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 2);
        // Back and forth: the queued occurrence (same title + next due date)
        // must not be created a second time.
        task_move(&conn, t.id, "progress", 1.0).unwrap();
        task_move(&conn, t.id, "done", 2.0).unwrap();
        task_move(&conn, t.id, "done", 3.0).unwrap();
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.len(), 2, "repeated done moves must not duplicate");
        let next = list.iter().find(|x| x.id != t.id).unwrap();
        assert_eq!(next.due_date.as_deref(), Some("2099-01-03"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
