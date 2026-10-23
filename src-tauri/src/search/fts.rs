use crate::error::Result;
use rusqlite::{params, Connection};

use super::types::{task_row_from, TaskRow, TASK_COLS};

pub(crate) fn fts_search(
    conn: &Connection,
    project_id: i64,
    match_query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT rel_path, name, snippet(files_fts, 3, '<<', '>>', '…', 12)
         FROM files_fts
         WHERE files_fts MATCH ?1 AND project_id = ?2
         ORDER BY rank
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![match_query, project_id.to_string(), limit as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn task_fts_search(
    conn: &Connection,
    project_id: i64,
    match_query: &str,
    limit: usize,
) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_COLS}
         FROM tasks_fts JOIN tasks ON tasks.id = tasks_fts.rowid
         WHERE tasks_fts MATCH ?1 AND tasks.project_id = ?2
         ORDER BY rank
         LIMIT ?3",
    ))?;
    let rows = stmt.query_map(params![match_query, project_id, limit as i64], task_row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn chat_fts_search(
    conn: &Connection,
    match_query: &str,
    limit: usize,
) -> Result<Vec<(i64, i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, session_id, snippet(messages_fts, 2, '<<', '>>', '…', 12)
         FROM messages_fts
         WHERE messages_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_query, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
