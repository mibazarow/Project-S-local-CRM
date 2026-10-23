// Task business rules: create/update/move/delete/restore operations and the
// validation behind them. SQL/data access lives in `repo`, DTO/snapshot types
// in `types`; the public surface is re-exported from the parent module.
use crate::error::{AppError, Result};
use crate::tasks::repo::{
    apply_timer_transition, dto, ensure_project, fts_sync, insert_task_row, is_archived, now_ms,
    task_restore_snapshot, task_snapshot,
};
use crate::tasks::types::{
    ChecklistItem, TaskDto, TaskPatch, TaskSnapshot, DEFAULT_DUE_TIME, MAX_CHECKLIST_ITEMS,
    STATUSES,
};
use rusqlite::{params, Connection, OptionalExtension};

// Test convenience over task_create_with defaults; production entry points
// pass the upgrade fields explicitly.
#[cfg(test)]
pub fn task_create(
    conn: &Connection,
    project_id: i64,
    raw_title: &str,
    status: Option<String>,
) -> Result<TaskDto> {
    task_create_with(conn, project_id, raw_title, status, None, None, None, None)
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
) -> Result<TaskDto> {
    ensure_project(conn, project_id)?;
    if is_archived(conn, project_id)? {
        return Err(AppError::Invalid(
            "Проект в архиве: создание задач недоступно. Сначала верните проект из архива.".into(),
        ));
    }
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
                            is_priority, checklist, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now','localtime'),
                 CASE WHEN ?3 = 'done' THEN datetime('now','localtime') ELSE NULL END,
                 ?7, ?8, CASE WHEN ?3 = 'progress' THEN ?9 ELSE NULL END)",
        params![project_id, title, status, due_date, due_time, position, is_priority, checklist_json, started_at],
    )?;
    let id = conn.last_insert_rowid();
    fts_sync(conn, id)?;
    dto(conn, id)
}

pub fn task_update(conn: &Connection, id: i64, patch: TaskPatch) -> Result<TaskDto> {
    let (mut title, mut comment, mut due_date, mut due_time, mut remind_at, mut is_priority,
         mut checklist_json) = conn
        .query_row(
            "SELECT title, comment, due_date, due_time, remind_at, is_priority, checklist
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

    conn.execute(
        "UPDATE tasks SET title = ?1, comment = ?2, due_date = ?3, due_time = ?4, remind_at = ?5,
                        is_priority = ?6, checklist = ?7
         WHERE id = ?8",
        params![title, comment, due_date, due_time, remind_at, is_priority, checklist_json, id],
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
    let exists: Option<i64> = conn
        .query_row("SELECT id FROM tasks WHERE id = ?1", params![id], |r| r.get(0))
        .optional()?;
    if exists.is_none() {
        return Err(AppError::Invalid(format!("task {id} not found")));
    }
    conn.execute(
        "UPDATE tasks SET status = ?1, position = ?2,
         closed_at = CASE WHEN ?1 = 'done'
                          THEN COALESCE(closed_at, datetime('now','localtime'))
                          ELSE NULL END
         WHERE id = ?3",
        params![status, position, id],
    )?;
    apply_timer_transition(conn, id, status)?;
    Ok((dto(conn, id)?, None))
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

fn check_status(status: &str) -> Result<()> {
    if STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "unknown task status \"{status}\", expected one of {STATUSES:?}"
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
