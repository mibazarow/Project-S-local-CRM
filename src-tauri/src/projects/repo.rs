// SQL/data access for projects: row reads and writes, the DTO assembly from a
// DB row plus the folder walk, and the delete transaction with its cleanup
// plumbing. Business rules, name/path validation and the on-disk lifecycle
// live in `service`, DTO/plan types in `types`; the public surface is
// re-exported from the parent module.
use crate::error::{AppError, Result};
use crate::paths;
use crate::projects::types::{
    parse_files_view, parse_task_columns, CreatedProject, DeletePlan, ProjectDbRow, ProjectDto,
    RenamePlan,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};

const TASK_COUNT_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1";
const OVERDUE_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1 AND status != 'done' AND due_date IS NOT NULL AND due_date || ' ' || due_time || ':00' < datetime('now','localtime')";

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

// SELECT of a single id column with one id-list IN clause; empty list = no rows.
fn ids_where_in(conn: &Connection, select: &str, values: &[i64]) -> Result<Vec<i64>> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!("{select} IN ({})", placeholders(values.len()));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn delete_rows_where_in(conn: &Connection, sql_head: &str, ids: &[i64]) -> Result<usize> {
    let mut total = 0;
    for chunk in ids.chunks(500) {
        let sql = format!("{sql_head} IN ({})", placeholders(chunk.len()));
        total += conn.execute(&sql, rusqlite::params_from_iter(chunk.iter()))?;
    }
    Ok(total)
}

// links has no FK: drop every row that names one of the deleted entities on
// either side (kind ids are stored as text).
fn delete_links_for(conn: &Connection, kind: &str, ids: &[String]) -> Result<()> {
    for chunk in ids.chunks(250) {
        let ph = placeholders(chunk.len());
        let sql = format!(
            "DELETE FROM links WHERE (from_kind = ?1 AND from_id IN ({ph}))
             OR (to_kind = ?1 AND to_id IN ({ph}))"
        );
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(1 + chunk.len() * 2);
        args.push(&kind);
        for v in chunk {
            args.push(v);
        }
        for v in chunk {
            args.push(v);
        }
        conn.execute(&sql, rusqlite::params_from_iter(args.iter()))?;
    }
    Ok(())
}

// The project folder must be a direct child of Projects/: canonicalize
// resolves traversal and symlinks, and the parent check rejects "/", the
// working dir, the Projects dir itself and any symlink pointing elsewhere.
// A folder that is already gone is allowed (DB-only cleanup) but only for a
// plain Projects/<name> path with no symlink at the spot. Returns the folder
// to remove (None = already gone) plus the path prefix used to match the
// trash/versions rows that referenced it.
fn resolve_delete_target(conn: &Connection, wd: &Path, id: i64) -> Result<(Option<PathBuf>, PathBuf)> {
    let stored: String = conn
        .query_row("SELECT path FROM projects WHERE id = ?1", params![id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| AppError::Invalid(format!("project {id} not found")))?;
    let projects = paths::projects_dir(wd);
    let candidate = PathBuf::from(&stored);
    match fs::symlink_metadata(&candidate) {
        Ok(meta) if meta.file_type().is_symlink() => Err(AppError::Invalid(format!(
            "project {id} folder is a symlink; refusing to delete it"
        ))),
        Ok(meta) if meta.is_dir() => {
            let projects_canon = projects.canonicalize()?;
            let canon = candidate.canonicalize()?;
            if canon.parent() != Some(projects_canon.as_path()) {
                return Err(AppError::Invalid(format!(
                    "project {id} path escapes the Projects folder"
                )));
            }
            Ok((Some(canon.clone()), canon))
        }
        Ok(_) => Err(AppError::Invalid(format!(
            "project {id} path is not a folder"
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if candidate.parent() != Some(projects.as_path()) {
                return Err(AppError::Invalid(format!(
                    "project {id} path escapes the Projects folder"
                )));
            }
            // Trash/version rows were recorded with the canonicalized root
            // while the folder existed: match against it best-effort.
            let match_root = match (projects.canonicalize(), candidate.file_name()) {
                (Ok(projects_canon), Some(name)) => projects_canon.join(name),
                _ => candidate.clone(),
            };
            Ok((None, match_root))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn delete_project(conn: &Connection, wd: &Path, id: i64) -> Result<DeletePlan> {
    if is_builtin(conn, id)? {
        return Err(AppError::Invalid(
            "the builtin project cannot be deleted".into(),
        ));
    }
    let (root, match_root) = resolve_delete_target(conn, wd, id)?;

    let task_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM tasks WHERE project_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let memory_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM memory WHERE project_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Schedules belong to the project through their linked task; a schedule's
    // harness session goes with it, together with every schedule created in
    // that session (same pairing as harness::delete_session).
    let mut schedule_ids = ids_where_in(
        conn,
        "SELECT id FROM schedules WHERE related_task_id",
        &task_ids,
    )?;
    let session_ids: Vec<i64> = {
        let mut ids = ids_where_in(
            conn,
            "SELECT session_id FROM schedules WHERE session_id IS NOT NULL AND related_task_id",
            &task_ids,
        )?;
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    if !session_ids.is_empty() {
        let mut owned = ids_where_in(conn, "SELECT id FROM schedules WHERE session_id", &session_ids)?;
        schedule_ids.append(&mut owned);
        schedule_ids.sort_unstable();
        schedule_ids.dedup();
    }

    // Chat/message ids of everything the projects-row cascade will remove
    // (project chats plus the harness sessions going away).
    let sess_ph = if session_ids.is_empty() {
        String::new()
    } else {
        format!(" OR session_id IN ({})", placeholders(session_ids.len()))
    };
    let chat_ids: Vec<i64> = {
        let sql = format!("SELECT id FROM chats WHERE project_id = ?1{sess_ph}");
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&id];
        for s in &session_ids {
            args.push(s);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let msg_ids: Vec<i64> = {
        let sql = format!(
            "SELECT m.id FROM messages m JOIN chats c ON c.id = m.chat_id \
             WHERE c.project_id = ?1{sess_ph}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&id];
        for s in &session_ids {
            args.push(s);
        }
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Harness search mirrors only cover session transcripts.
    let mirror_msg_ids: Vec<i64> = {
        let sql = "SELECT m.id FROM messages m JOIN chats c ON c.id = m.chat_id \
                   WHERE c.kind = 'harness' AND c.session_id IS NOT NULL";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let all: Vec<i64> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        all.into_iter().filter(|m| msg_ids.contains(m)).collect()
    };

    // Trash/version snapshots of the project's files and tasks: a restore
    // would recreate the deleted folder, so they go with the project. The
    // path match runs in Rust (LIKE wildcards in project names would over-
    // match other projects' rows).
    let root_disp = match_root.display().to_string();
    let root_prefix = format!("{root_disp}/");
    let task_keys: std::collections::HashSet<String> =
        task_ids.iter().map(|t| format!("task:{t}")).collect();
    let trash_rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, scope, original_path, stored_path FROM trash WHERE scope IN ('project', 'task')",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_, scope, original, stored) = row?;
            let mine = (scope == "task" && task_keys.contains(&original))
                || (scope == "project" && (original == root_disp || original.starts_with(&root_prefix)));
            if mine {
                out.push((id_, stored));
            }
        }
        out
    };
    let version_rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, path, stored_path FROM versions WHERE scope = 'project'")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id_, path, stored) = row?;
            if path == root_disp || path.starts_with(&root_prefix) {
                out.push((id_, stored));
            }
        }
        out
    };
    let trash_ids: Vec<i64> = trash_rows.iter().map(|(id_, _)| *id_).collect();
    let version_ids: Vec<i64> = version_rows.iter().map(|(id_, _)| *id_).collect();

    let tx = conn.unchecked_transaction()?;

    // links: no FK, so every row naming a deleted entity on either side.
    delete_links_for(&tx, "project", &[id.to_string()])?;
    delete_links_for(&tx, "task", &task_ids.iter().map(i64::to_string).collect::<Vec<_>>())?;
    delete_links_for(&tx, "schedule", &schedule_ids.iter().map(i64::to_string).collect::<Vec<_>>())?;
    delete_links_for(&tx, "chat", &chat_ids.iter().map(i64::to_string).collect::<Vec<_>>())?;
    delete_links_for(&tx, "message", &msg_ids.iter().map(i64::to_string).collect::<Vec<_>>())?;
    let file_prefix = format!("{id}:%");
    tx.execute(
        "DELETE FROM links WHERE (from_kind = 'file' AND from_id LIKE ?1) OR (to_kind = 'file' AND to_id LIKE ?1)",
        params![file_prefix],
    )?;

    // Per-conversation TODO plans of the project chat scope (no FK by design).
    tx.execute("DELETE FROM todos WHERE scope_kind = 'project' AND scope_id = ?1", params![id])?;

    // Search mirrors without cascade: tasks_fts (contentless, keyed by task
    // rowid), the file index legs, and the memory legs.
    for t in &task_ids {
        tx.execute("DELETE FROM tasks_fts WHERE rowid = ?1", params![t])?;
    }
    tx.execute("DELETE FROM files_fts WHERE project_id = ?1", params![id.to_string()])?;
    tx.execute(
        "DELETE FROM vec_items WHERE rowid IN (SELECT vec_rowid FROM file_vectors WHERE project_id = ?1 AND vec_rowid IS NOT NULL)",
        params![id],
    )?;
    tx.execute("DELETE FROM file_vectors WHERE project_id = ?1", params![id])?;
    tx.execute("DELETE FROM indexed_files WHERE project_id = ?1", params![id])?;
    for m in &memory_ids {
        tx.execute("DELETE FROM memory_fts WHERE rowid = ?1", params![m])?;
        tx.execute("DELETE FROM memory_vectors WHERE memory_id = ?1", params![m])?;
        tx.execute("DELETE FROM vec_memory WHERE rowid = ?1", params![m])?;
    }
    let deleted_memory = tx.execute("DELETE FROM memory WHERE project_id = ?1", params![id])?;

    // Harness session transcripts leave mirrors behind (messages_fts /
    // message_vectors / vec_messages have no FK): clear them first, then the
    // sessions (cascade: chats → messages, summaries, attachments) and the
    // project's schedules.
    for m in &mirror_msg_ids {
        tx.execute("DELETE FROM messages_fts WHERE message_id = ?1", params![m])?;
        tx.execute("DELETE FROM message_vectors WHERE message_id = ?1", params![m])?;
        tx.execute("DELETE FROM vec_messages WHERE rowid = ?1", params![m])?;
    }
    let deleted_sessions = delete_rows_where_in(&tx, "DELETE FROM sessions WHERE id", &session_ids)?;
    let deleted_schedules = delete_rows_where_in(&tx, "DELETE FROM schedules WHERE id", &schedule_ids)?;

    // Trash and version history of the project: rows now, stored files after
    // the commit (a rollback must not have destroyed the snapshots).
    delete_rows_where_in(&tx, "DELETE FROM trash WHERE id", &trash_ids)?;
    delete_rows_where_in(&tx, "DELETE FROM versions WHERE id", &version_ids)?;

    // Undo entries that would restore into the deleted project or reference a
    // purged trash/version row: no restore path survives the deletion.
    tx.execute("DELETE FROM undo_log WHERE json_extract(payload, '$.projectId') = ?1", params![id])?;
    delete_rows_where_in(
        &tx,
        "DELETE FROM undo_log WHERE json_extract(payload, '$.trashId')",
        &trash_ids,
    )?;
    delete_rows_where_in(
        &tx,
        "DELETE FROM undo_log WHERE json_extract(payload, '$.versionId')",
        &version_ids,
    )?;

    // projects cascades tasks, chats (→ messages → attachments, summaries).
    let deleted = tx.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
    if deleted != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    tx.commit()?;

    Ok(DeletePlan {
        root,
        trash_stored: trash_rows
            .into_iter()
            .map(|(_, stored)| stored)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect(),
        version_stored: version_rows
            .into_iter()
            .map(|(_, stored)| stored)
            .map(PathBuf::from)
            .collect(),
        deleted_sessions,
        deleted_schedules,
        deleted_memory,
    })
}

// Filesystem half of the deletion, run only after the DB commit. A folder
// that cannot be removed is reported without resurrecting the DB record.
pub fn delete_project_fs(wd: &Path, plan: &DeletePlan) -> Result<()> {
    if let Some(root) = &plan.root {
        fs::remove_dir_all(root).map_err(|e| {
            AppError::Io(format!(
                "project folder {} could not be deleted: {e}",
                root.display()
            ))
        })?;
    }
    let trash = paths::trash_dir(wd);
    for stored in &plan.trash_stored {
        if !stored.starts_with(&trash) {
            continue;
        }
        if let Some(dir) = stored.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }
    let versions = paths::versions_dir(wd);
    for stored in &plan.version_stored {
        if !stored.starts_with(&versions) {
            continue;
        }
        let _ = fs::remove_file(stored);
    }
    Ok(())
}

// DB-only part of the project list: rows without the on-disk file count.
// The command collects these under the DB mutex, then walks the folders
// outside the lock (dto_from_row).
pub(crate) fn list_project_rows(conn: &Connection) -> Result<Vec<(i64, ProjectDbRow)>> {
    let mut ids = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY builtin DESC, position, id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            ids.push(row?);
        }
    }
    ids.iter().map(|id| Ok((*id, db_row(conn, *id)?))).collect()
}

pub fn list_projects(conn: &Connection) -> Result<Vec<ProjectDto>> {
    Ok(list_project_rows(conn)?
        .into_iter()
        .map(|(id, row)| dto_from_row(id, row))
        .collect())
}

pub(crate) fn create_project_db(
    conn: &Connection,
    made: &CreatedProject,
) -> Result<(i64, ProjectDbRow)> {
    conn.execute(
        "INSERT INTO projects (name, path, created_at, position)
         VALUES (?1, ?2, datetime('now','localtime'),
                 (SELECT COALESCE(MAX(position), 0) + 1 FROM projects))",
        params![made.name, made.root.display().to_string()],
    )?;
    let id = conn.last_insert_rowid();
    Ok((id, db_row(conn, id)?))
}

pub(crate) fn rename_apply(conn: &Connection, id: i64, plan: &RenamePlan) -> Result<ProjectDbRow> {
    let updated = conn.execute(
        "UPDATE projects SET name = ?1, path = ?2 WHERE id = ?3",
        params![plan.name, plan.new_root.display().to_string(), id],
    )?;
    if updated != 1 {
        return Err(AppError::Db(format!("project {id} not found")));
    }
    db_row(conn, id)
}

pub fn is_builtin(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT builtin FROM projects WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|v| v != 0)
        .unwrap_or(false))
}

pub fn builtin_project_id(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM projects WHERE builtin = 1", [], |r| r.get(0))
        .optional()?)
}

pub fn touch_project_opened(conn: &Connection, id: i64) -> Result<()> {
    let updated = conn.execute(
        "UPDATE projects SET opened_at = datetime('now','localtime') WHERE id = ?1",
        params![id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    Ok(())
}

// Manual list ordering: the frontend sends a fractional position (midpoint
// between neighbors), so a move never renumbers the whole list.
pub fn move_project(conn: &Connection, id: i64, position: f64) -> Result<()> {
    let updated = conn.execute(
        "UPDATE projects SET position = ?2 WHERE id = ?1",
        params![id, position],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    Ok(())
}

// First project in list order: the quick-capture task fallback when the
// capture modal has no explicit project selected.
pub fn first_project_id(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM projects ORDER BY builtin DESC, position, id LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn get_project(conn: &Connection, id: i64) -> Result<ProjectDto> {    let exists: i64 = conn.query_row("SELECT count(*) FROM projects WHERE id = ?1", params![id], |r| r.get(0))?;
    if exists == 0 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    dto(conn, id)
}

pub(crate) fn db_row(conn: &Connection, id: i64) -> Result<ProjectDbRow> {
    let (name, path, created_at, opened_at, position, task_columns, files_view, archived_at, builtin, has_active_tasks) = conn.query_row(
        "SELECT name, path, created_at, opened_at, position, task_columns, files_view, archived_at, builtin,
                EXISTS(SELECT 1 FROM tasks WHERE project_id = ?1 AND status = 'progress')
         FROM projects WHERE id = ?1",
        params![id],
        |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, i64>(8)? != 0,
            r.get::<_, bool>(9)?,
        )),
    )?;
    let task_count = conn.query_row(TASK_COUNT_SQL, params![id], |r| r.get(0))?;
    let overdue_count = conn.query_row(OVERDUE_SQL, params![id], |r| r.get(0))?;
    Ok(ProjectDbRow {
        name,
        path,
        created_at,
        opened_at,
        position,
        task_columns,
        files_view,
        archived_at,
        builtin,
        task_count,
        overdue_count,
        has_active_tasks,
    })
}

// Assembles the DTO from DB parts; the file count walks the project folder,
// so callers that hold the DB mutex must drop it before calling this.
pub(crate) fn dto_from_row(id: i64, row: ProjectDbRow) -> ProjectDto {
    ProjectDto {
        id,
        name: row.name,
        path: row.path.clone(),
        created_at: row.created_at,
        opened_at: row.opened_at,
        position: row.position,
        file_count: count_files(Path::new(&row.path)),
        task_count: row.task_count,
        overdue_count: row.overdue_count,
        has_active_tasks: row.has_active_tasks,
        task_columns: parse_task_columns(&row.task_columns),
        files_view: parse_files_view(&row.files_view),
        archived: row.archived_at.is_some(),
        archived_at: row.archived_at,
        builtin: row.builtin,
    }
}

pub(crate) fn dto(conn: &Connection, id: i64) -> Result<ProjectDto> {
    Ok(dto_from_row(id, db_row(conn, id)?))
}

fn count_files(root: &Path) -> i64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if entry.file_name() != ".DS_Store" {
                total += 1;
            }
        }
    }
    total
}
