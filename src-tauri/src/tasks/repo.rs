// SQL/data access for tasks: row reads and writes, the FTS mirror, the
// snapshot/restore plumbing and the shared clock helpers. Business rules and
// validation live in `service`, DTO/snapshot types in `types`; the public
// surface is re-exported from the parent module.
use crate::error::{AppError, Result};
use crate::tasks::types::{ChecklistItem, TaskDto, TaskSnapshot};
use rusqlite::{params, Connection, OptionalExtension};

// Kanban column order: new -> progress -> done.
const ORDER_SQL: &str = "ORDER BY CASE status WHEN 'new' THEN 0 WHEN 'progress' THEN 1 ELSE 2 END, position, id";

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

pub(crate) fn insert_task_row(conn: &Connection, snapshot: &TaskSnapshot, with_id: bool) -> Result<()> {
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

// tasks_fts mirrors title/comment keyed by task id (SPEC 4.4); search joins
// tasks for scoping and status, so closed tasks stay findable. The contentless
// table rejects UPDATE, so re-sync is delete-then-insert; the delete also
// clears a rowid reused by a newer task. task_move changes only status (kept
// in tasks, not in the mirror), so it needs no sync. The legacy tags column of
// the mirror is written empty: tags are no longer part of the task model.
pub(crate) fn fts_sync(conn: &Connection, id: i64) -> Result<()> {
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

pub(crate) fn dto(conn: &Connection, id: i64) -> Result<TaskDto> {
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

fn now(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT datetime('now','localtime')", [], |r| r.get(0))?)
}

// Wall-clock epoch ms (UTC): the timer never parses date/time strings and
// keeps running while the app is closed (SPEC 4.3).
pub(crate) fn now_ms(conn: &Connection) -> Result<i64> {
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
pub(crate) fn apply_timer_transition(conn: &Connection, id: i64, new_status: &str) -> Result<()> {
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
pub(crate) fn is_overdue(status: &str, due_date: Option<&str>, due_time: &str, now: &str) -> bool {
    status != "done"
        && due_date.is_some_and(|d| {
            let due = format!("{d} {due_time}:00");
            due.as_str() < now
        })
}

fn parse_checklist(raw: &str) -> Result<Vec<ChecklistItem>> {
    Ok(serde_json::from_str(raw).unwrap_or_default())
}

pub(crate) fn ensure_project(conn: &Connection, id: i64) -> Result<()> {
    let exists: i64 =
        conn.query_row("SELECT count(*) FROM projects WHERE id = ?1", params![id], |r| r.get(0))?;
    if exists == 0 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    Ok(())
}

pub(crate) fn is_archived(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT archived_at IS NOT NULL FROM projects WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?)
}
