use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub rel_path: String,
    pub name: String,
    // "file" or "task"; task hits carry an empty rel_path.
    pub kind: String,
    pub score: f64,
    pub snippet: String,
    pub match_types: Vec<String>,
    // File mtime in epoch milliseconds, from indexed_files (0 for task hits).
    pub modified: i64,
    // Task hits (SPEC 4.4): identify the task row for the UI.
    pub task_id: Option<i64>,
    pub status: Option<String>,
    pub overdue: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatHit {
    pub session_id: i64,
    pub title: Option<String>,
    pub last_active: String,
    pub score: f64,
    pub message_id: i64,
    // Matched-message context (kept for the ⌘K palette snippet).
    pub snippet: String,
    // Chat-level identity: the latest checkpoint summary, else the first user
    // message, so the agent can tell the user which chat a hit is.
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalHit {
    // "project" | "file" | "task" | "note" | "chat"
    pub kind: String,
    // Entity id: project/task/session id; files_fts rowid for file/note hits.
    pub id: i64,
    pub title: String,
    pub subtitle: String,
    pub snippet: String,
    pub score: f64,
    pub project_id: Option<i64>,
    pub session_id: Option<i64>,
    pub task_id: Option<i64>,
    pub rel_path: Option<String>,
}

// Task row joined into search: status/overdue come from tasks, so closed
// tasks are included and status/date filtering stays possible via the join.
#[derive(Debug, Clone)]
pub(crate) struct TaskRow {
    pub(crate) id: i64,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) comment: Option<String>,
    pub(crate) overdue: bool,
}

pub(crate) const TASK_COLS: &str = "tasks.id, tasks.title, tasks.status, tasks.comment, \
     (tasks.due_date IS NOT NULL AND tasks.due_date || ' ' || tasks.due_time || ':00' < datetime('now','localtime') AND tasks.status <> 'done')";

pub(crate) fn task_row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: row.get(0)?,
        title: row.get(1)?,
        status: row.get(2)?,
        comment: row.get(3)?,
        overdue: row.get::<_, i64>(4)? != 0,
    })
}
