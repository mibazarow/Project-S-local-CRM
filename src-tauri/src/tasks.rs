use crate::error::{AppError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const STATUSES: [&str; 3] = ["new", "progress", "done"];

// Kanban column order: new -> progress -> done.
const ORDER_SQL: &str = "ORDER BY CASE status WHEN 'new' THEN 0 WHEN 'progress' THEN 1 ELSE 2 END, position, id";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDto {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub status: String,
    pub comment: Option<String>,
    pub due_date: Option<String>,
    pub remind_at: Option<String>,
    pub tags: Vec<String>,
    pub position: f64,
    pub created_at: String,
    pub closed_at: Option<String>,
    pub overdue: bool,
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
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskPatch {
    pub title: Option<String>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub comment: Option<Option<String>>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub due_date: Option<Option<String>>,
    #[serde(deserialize_with = "opt_opt_str::deserialize")]
    pub remind_at: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
}

pub fn tasks_list(conn: &Connection, project_id: i64) -> Result<Vec<TaskDto>> {
    ensure_project(conn, project_id)?;
    let today = today(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, status, comment, due_date, remind_at, tags, position, created_at, closed_at
         FROM tasks WHERE project_id = ?1 {ORDER_SQL}"
    ))?;
    let mut rows = stmt.query(params![project_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let status: String = row.get(2)?;
        let due_date: Option<String> = row.get(4)?;
        let overdue =
            due_date.as_deref().is_some_and(|d| d < today.as_str()) && status != "done";
        out.push(TaskDto {
            id: row.get(0)?,
            project_id,
            title: row.get(1)?,
            overdue,
            tags: parse_tags(&row.get::<_, String>(6)?)?,
            position: row.get(7)?,
            created_at: row.get(8)?,
            closed_at: row.get(9)?,
            comment: row.get(3)?,
            due_date,
            remind_at: row.get(5)?,
            status,
        });
    }
    Ok(out)
}

pub fn task_create(
    conn: &Connection,
    project_id: i64,
    raw_title: &str,
    status: Option<String>,
) -> Result<TaskDto> {
    ensure_project(conn, project_id)?;
    let title = raw_title.trim();
    if title.is_empty() {
        return Err(AppError::Invalid("task title is empty".into()));
    }
    let status = status.unwrap_or_else(|| "new".into());
    check_status(&status)?;
    let position: f64 = conn.query_row(
        "SELECT COALESCE(MAX(position), 0) + 1 FROM tasks WHERE project_id = ?1 AND status = ?2",
        params![project_id, status],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO tasks (project_id, title, status, tags, position, created_at, closed_at)
         VALUES (?1, ?2, ?3, '[]', ?4, datetime('now','localtime'),
                  CASE WHEN ?3 = 'done' THEN datetime('now','localtime') ELSE NULL END)",
        params![project_id, title, status, position],
    )?;
    let id = conn.last_insert_rowid();
    fts_sync(conn, id)?;
    dto(conn, id)
}

pub fn task_update(conn: &Connection, id: i64, patch: TaskPatch) -> Result<TaskDto> {
    let (mut title, mut comment, mut due_date, mut remind_at, mut tags_json) = conn
        .query_row(
            "SELECT title, comment, due_date, remind_at, tags FROM tasks WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
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
    if let Some(new_remind) = patch.remind_at {
        if let Some(r) = &new_remind {
            check_remind_at(r)?;
        }
        remind_at = new_remind;
    }
    if let Some(tags) = patch.tags {
        let cleaned: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        tags_json = serde_json::to_string(&cleaned)?;
    }

    conn.execute(
        "UPDATE tasks SET title = ?1, comment = ?2, due_date = ?3, remind_at = ?4, tags = ?5
         WHERE id = ?6",
        params![title, comment, due_date, remind_at, tags_json, id],
    )?;
    fts_sync(conn, id)?;
    dto(conn, id)
}

pub fn task_move(conn: &Connection, id: i64, status: &str, position: f64) -> Result<TaskDto> {
    check_status(status)?;
    let updated = conn.execute(
        "UPDATE tasks SET status = ?1, position = ?2,
         closed_at = CASE WHEN ?1 = 'done'
                          THEN COALESCE(closed_at, datetime('now','localtime'))
                          ELSE NULL END
         WHERE id = ?3",
        params![status, position, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("task {id} not found")));
    }
    dto(conn, id)
}

// Full tasks row as JSON, snapshotted into the trash table before delete
// (kind='task', no file on disk); re-inserted by task_restore_from_trash.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskSnapshot {
    id: i64,
    project_id: i64,
    title: String,
    status: String,
    comment: Option<String>,
    due_date: Option<String>,
    remind_at: Option<String>,
    tags: String,
    position: f64,
    created_at: String,
    closed_at: Option<String>,
}

// Returns the owning project id so the caller can re-export it. The row is
// snapshotted into the trash table first, so a deleted task stays restorable.
pub fn task_delete(conn: &Connection, id: i64) -> Result<i64> {
    let snapshot: TaskSnapshot = conn
        .query_row(
            "SELECT id, project_id, title, status, comment, due_date, remind_at,
                    tags, position, created_at, closed_at
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
                    remind_at: r.get(6)?,
                    tags: r.get(7)?,
                    position: r.get(8)?,
                    created_at: r.get(9)?,
                    closed_at: r.get(10)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid(format!("task {id} not found")))?;
    let project_id = snapshot.project_id;
    crate::reliability::trash_put_meta(
        conn,
        "task",
        "task",
        &format!("task:{id}"),
        &snapshot.title,
        &serde_json::to_string(&snapshot)?,
    )?;
    conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
    fts_sync(conn, id)?;
    Ok(project_id)
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
        conn.execute(
            "INSERT INTO tasks (id, project_id, title, status, comment, due_date, remind_at,
                                tags, position, created_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                snapshot.id, snapshot.project_id, snapshot.title, snapshot.status,
                snapshot.comment, snapshot.due_date, snapshot.remind_at, snapshot.tags,
                snapshot.position, snapshot.created_at, snapshot.closed_at
            ],
        )?;
    } else {
        conn.execute(
            "INSERT INTO tasks (project_id, title, status, comment, due_date, remind_at,
                                tags, position, created_at, closed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                snapshot.project_id, snapshot.title, snapshot.status,
                snapshot.comment, snapshot.due_date, snapshot.remind_at, snapshot.tags,
                snapshot.position, snapshot.created_at, snapshot.closed_at
            ],
        )?;
    }
    fts_sync(conn, conn.last_insert_rowid())?;
    Ok(snapshot.project_id)
}

// tasks_fts mirrors title/comment/tags keyed by task id (SPEC 4.4); search
// joins tasks for scoping and status, so closed tasks stay findable. The
// contentless table rejects UPDATE, so re-sync is delete-then-insert; the
// delete also clears a rowid reused by a newer task. task_move changes only
// status (kept in tasks, not in the mirror), so it needs no sync.
fn fts_sync(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM tasks_fts WHERE rowid = ?1", params![id])?;
    let row = conn
        .query_row(
            "SELECT title, comment, tags FROM tasks WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((title, comment, tags)) = row {
        conn.execute(
            "INSERT INTO tasks_fts (rowid, title, comment, tags) VALUES (?1, ?2, ?3, ?4)",
            params![id, title, comment, tags],
        )?;
    }
    Ok(())
}

fn dto(conn: &Connection, id: i64) -> Result<TaskDto> {
    let today = today(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT project_id, title, status, comment, due_date, remind_at, tags, position, created_at, closed_at
         FROM tasks WHERE id = ?1 {ORDER_SQL}"
    ))?;
    let mut rows = stmt.query(params![id])?;
    let row = rows
        .next()?
        .ok_or_else(|| AppError::Invalid(format!("task {id} not found")))?;
    let project_id: i64 = row.get(0)?;
    let status: String = row.get(2)?;
    let due_date: Option<String> = row.get(4)?;
    let overdue = due_date.as_deref().is_some_and(|d| d < today.as_str()) && status != "done";
    Ok(TaskDto {
        id,
        project_id,
        title: row.get(1)?,
        overdue,
        tags: parse_tags(&row.get::<_, String>(6)?)?,
        position: row.get(7)?,
        created_at: row.get(8)?,
        closed_at: row.get(9)?,
        comment: row.get(3)?,
        due_date,
        remind_at: row.get(5)?,
        status,
    })
}

fn today(conn: &Connection) -> Result<String> {
    Ok(conn.query_row("SELECT date('now','localtime')", [], |r| r.get(0))?)
}

fn parse_tags(raw: &str) -> Result<Vec<String>> {
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
                tags: Some(vec![" one ".into(), "".into(), "two".into()]),
            },
        )
        .unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(updated.comment.as_deref(), Some("note"));
        assert_eq!(updated.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(updated.remind_at.as_deref(), Some("2099-01-02T10:30"));
        assert_eq!(updated.tags, vec!["one", "two"]);

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
                tags: Some(vec!["редизайн".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fts_count("старое"), 0, "old text must be gone after update");
        assert_eq!(fts_count("новое"), 1);
        assert_eq!(fts_count("комментарий"), 1);
        assert_eq!(fts_count("редизайн"), 1);

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
                tags: Some(vec!["x".into()]),
                ..Default::default()
            },
        )
        .unwrap();

        let empty: TaskPatch = serde_json::from_str("{}").unwrap();
        let kept = task_update(&conn, t.id, empty).unwrap();
        assert_eq!(kept.comment.as_deref(), Some("note"));
        assert_eq!(kept.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(kept.remind_at.as_deref(), Some("2099-01-02T08:00"));
        assert_eq!(kept.tags, vec!["x"]);

        let cleared: TaskPatch =
            serde_json::from_str(r#"{"comment": null, "dueDate": null, "remindAt": null}"#).unwrap();
        let cleared = task_update(&conn, t.id, cleared).unwrap();
        assert_eq!(cleared.comment, None);
        assert_eq!(cleared.due_date, None);
        assert_eq!(cleared.remind_at, None);
        assert_eq!(cleared.tags, vec!["x"]);

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
}
