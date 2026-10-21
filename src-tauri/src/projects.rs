use crate::error::{AppError, Result};
use crate::paths;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDto {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub opened_at: Option<String>,
    pub position: f64,
    pub file_count: i64,
    pub task_count: i64,
    pub overdue_count: i64,
    pub has_active_tasks: bool,
    pub task_columns: [String; 3],
    pub files_view: String,
    pub archived: bool,
    pub archived_at: Option<String>,
}

const TASK_COUNT_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1";
const OVERDUE_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1 AND status != 'done' AND due_date IS NOT NULL AND due_date || ' ' || due_time || ':00' < datetime('now','localtime')";

// Per-project kanban column names (v31): a JSON array of exactly 3 strings.
pub const DEFAULT_TASK_COLUMNS: [&str; 3] = ["Новые", "В работе", "Готово"];
pub const MAX_TASK_COLUMN_NAME_CHARS: usize = 40;

fn parse_task_columns(raw: &str) -> [String; 3] {
    serde_json::from_str::<Vec<String>>(raw)
        .ok()
        .filter(|v| v.len() == 3)
        .map(|v| [v[0].clone(), v[1].clone(), v[2].clone()])
        .unwrap_or_else(|| DEFAULT_TASK_COLUMNS.map(str::to_owned))
}

// Kanban column names: exactly 3, trimmed, non-empty, capped so a column
// header stays one line in the UI.
pub fn set_task_columns(conn: &Connection, id: i64, columns: Vec<String>) -> Result<ProjectDto> {
    if columns.len() != 3 {
        return Err(AppError::Invalid(format!(
            "task columns must be exactly 3 names, got {}",
            columns.len()
        )));
    }
    let cleaned: Vec<String> = columns.iter().map(|c| c.trim().to_owned()).collect();
    for name in &cleaned {
        if name.is_empty() {
            return Err(AppError::Invalid("task column name is empty".into()));
        }
        if name.chars().count() > MAX_TASK_COLUMN_NAME_CHARS {
            return Err(AppError::Invalid(format!(
                "task column name is limited to {MAX_TASK_COLUMN_NAME_CHARS} characters"
            )));
        }
    }
    let updated = conn.execute(
        "UPDATE projects SET task_columns = ?1 WHERE id = ?2",
        params![serde_json::to_string(&cleaned)?, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    dto(conn, id)
}

// Per-project file view mode (SPEC 4.2): "list" | "grid". Unknown values
// normalize to the default on write, and corrupt stored values fall back to
// it on read, so the DTO always carries a valid mode.
pub const DEFAULT_FILES_VIEW: &str = "list";

fn parse_files_view(raw: &str) -> String {
    if raw == "grid" {
        "grid".to_owned()
    } else {
        DEFAULT_FILES_VIEW.to_owned()
    }
}

fn normalize_files_view(raw: &str) -> String {
    parse_files_view(&raw.trim().to_ascii_lowercase())
}

// Persist the per-project file view mode; unknown values fall back to the
// default instead of erroring (the UI only ever sends the two known modes).
pub fn set_files_view(conn: &Connection, id: i64, raw: &str) -> Result<ProjectDto> {
    let updated = conn.execute(
        "UPDATE projects SET files_view = ?1 WHERE id = ?2",
        params![normalize_files_view(raw), id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    dto(conn, id)
}

pub fn set_archived(conn: &Connection, id: i64, archived: bool) -> Result<ProjectDto> {
    let updated = conn.execute(
        "UPDATE projects SET archived_at = CASE WHEN ?1 THEN datetime('now','localtime') ELSE NULL END
         WHERE id = ?2",
        params![archived, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    dto(conn, id)
}

// Permanent project deletion (Q.md item 1): no Trash, no undo path. The DB
// cleanup runs in one transaction (delete_project); the filesystem cleanup
// (delete_project_fs) runs only after the commit, so a failed folder removal
// never resurrects the DB record.

// What delete_project collected for the post-commit filesystem cleanup.
#[derive(Debug)]
pub struct DeletePlan {
    // Canonicalized project folder to remove; None when it is already gone.
    root: Option<PathBuf>,
    // stored_path of the purged trash rows (the <id> store dir is removed).
    trash_stored: Vec<PathBuf>,
    // stored_path of the purged version snapshots.
    version_stored: Vec<PathBuf>,
    pub deleted_sessions: usize,
    pub deleted_schedules: usize,
    pub deleted_memory: usize,
}

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

// Project names double as folder names, so keep them filesystem-safe.
pub fn sanitize_project_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AppError::Invalid("project name is empty".into()));
    }
    if name.starts_with('.') {
        return Err(AppError::Invalid("project name must not start with a dot".into()));
    }
    if name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control)
    {
        return Err(AppError::Invalid("project name contains forbidden characters".into()));
    }
    Ok(name.to_owned())
}

// Canonical project folder, guaranteed to stay under Projects/ of the working dir.
pub(crate) fn project_root(conn: &Connection, wd: &Path, id: i64) -> Result<PathBuf> {
    let stored: String = conn
        .query_row("SELECT path FROM projects WHERE id = ?1", params![id], |r| r.get(0))
        .map_err(|_| AppError::Invalid(format!("project {id} not found")))?;
    let projects = paths::projects_dir(wd).canonicalize()?;
    let root = PathBuf::from(stored)
        .canonicalize()
        .map_err(|_| AppError::Invalid(format!("project {id} folder is missing on disk")))?;
    if !root.starts_with(&projects) {
        return Err(AppError::Invalid(format!(
            "project {id} path escapes the Projects folder"
        )));
    }
    Ok(root)
}

pub fn project_path(conn: &Connection, wd: &Path, id: i64) -> Result<String> {
    Ok(project_root(conn, wd, id)?.display().to_string())
}

// DB-only part of the project list: rows without the on-disk file count.
// The command collects these under the DB mutex, then walks the folders
// outside the lock (dto_from_row).
pub(crate) fn list_project_rows(conn: &Connection) -> Result<Vec<(i64, ProjectDbRow)>> {
    let mut ids = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY position, id")?;
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

// Everything the create flow does on disk, with no DB touched: the command
// runs this outside the DB mutex, then calls create_project_db for the single
// INSERT. `undo_create` removes only what this call created.
#[derive(Debug)]
pub(crate) struct CreatedProject {
    name: String,
    root: PathBuf,
    projects_dir: PathBuf,
    created_projects_dir: bool,
}

pub(crate) fn create_project_fs(wd: &Path, raw_name: &str) -> Result<CreatedProject> {
    let name = sanitize_project_name(raw_name)?;
    let projects = paths::projects_dir(wd);
    let created_projects_dir = !projects.exists();
    fs::create_dir_all(&projects)?;
    let root = projects.join(&name);
    // create_dir (not create_dir_all) reserves the folder atomically: an
    // existing entry maps to the same "already exists" error as the old
    // exists() check, without a TOCTOU window between check and create.
    if let Err(e) = fs::create_dir(&root) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(AppError::Invalid(format!("project \"{name}\" already exists")));
        }
        return Err(e.into());
    }
    fs::create_dir_all(root.join(paths::PROJECT_TASKS))?;
    fs::create_dir_all(root.join(paths::PROJECT_INBOX))?;
    fs::write(root.join(paths::DATAS_MD), "")?;
    Ok(CreatedProject {
        name,
        root,
        projects_dir: projects,
        created_projects_dir,
    })
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

pub(crate) fn undo_create(made: &CreatedProject) {
    let _ = fs::remove_dir_all(&made.root);
    if made.created_projects_dir {
        // Only if still empty: another concurrent create may own its content.
        let _ = fs::remove_dir(&made.projects_dir);
    }
}

pub fn create_project(conn: &Connection, wd: &Path, raw_name: &str) -> Result<ProjectDto> {
    let made = create_project_fs(wd, raw_name)?;
    let (id, row) = match create_project_db(conn, &made) {
        Ok(v) => v,
        Err(e) => {
            undo_create(&made);
            return Err(e);
        }
    };
    Ok(dto_from_row(id, row))
}

// Everything rename_project does except the folder rename and the tree walk:
// the command resolves under a short DB read, renames outside the DB mutex,
// then applies the single UPDATE. On DB failure the caller renames back with
// rename_rollback (same compensation as before).
#[derive(Debug)]
pub(crate) struct RenamePlan {
    name: String,
    old_root: PathBuf,
    new_root: PathBuf,
}

pub(crate) fn rename_plan(conn: &Connection, wd: &Path, id: i64, raw_name: &str) -> Result<RenamePlan> {
    let name = sanitize_project_name(raw_name)?;
    let old_root = project_root(conn, wd, id)?;
    let new_root = paths::projects_dir(wd).canonicalize()?.join(&name);
    if new_root.exists() {
        return Err(AppError::Invalid(format!("project \"{name}\" already exists")));
    }
    Ok(RenamePlan {
        name,
        old_root,
        new_root,
    })
}

pub(crate) fn rename_move(plan: &RenamePlan) -> Result<()> {
    fs::rename(&plan.old_root, &plan.new_root)?;
    Ok(())
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

pub(crate) fn rename_rollback(plan: &RenamePlan) {
    let _ = fs::rename(&plan.new_root, &plan.old_root);
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
            "SELECT id FROM projects ORDER BY position, id LIMIT 1",
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

// Everything a project DTO needs from the DB, without the filesystem walk.
#[derive(Debug)]
pub(crate) struct ProjectDbRow {
    name: String,
    path: String,
    created_at: String,
    opened_at: Option<String>,
    position: f64,
    task_columns: String,
    files_view: String,
    archived_at: Option<String>,
    task_count: i64,
    overdue_count: i64,
    has_active_tasks: bool,
}

fn db_row(conn: &Connection, id: i64) -> Result<ProjectDbRow> {
    let (name, path, created_at, opened_at, position, task_columns, files_view, archived_at, has_active_tasks) = conn.query_row(
        "SELECT name, path, created_at, opened_at, position, task_columns, files_view, archived_at,
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
            r.get::<_, bool>(8)?,
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
    }
}

fn dto(conn: &Connection, id: i64) -> Result<ProjectDto> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{links, memory, reliability, tasks};
    use crate::db::schema;
    use crate::onboarding::temp_test_dir;

    fn setup(tag: &str) -> (PathBuf, Connection) {
        let wd = temp_test_dir(tag);
        fs::create_dir_all(paths::projects_dir(&wd)).unwrap();
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::run_migrations(&conn).unwrap();
        (wd, conn)
    }

    #[test]
    fn create_project_makes_folder_datas_tasks_inbox_and_db_row() {
        let (wd, conn) = setup("proj-create");
        let dto = create_project(&conn, &wd, "  example.ru  ").unwrap();
        assert_eq!(dto.name, "example.ru");
        let root = paths::projects_dir(&wd).join("example.ru");
        assert!(root.is_dir());
        assert!(root.join(paths::DATAS_MD).is_file());
        assert!(root.join(paths::PROJECT_TASKS).is_dir());
        assert!(root.join(paths::PROJECT_INBOX).is_dir());
        assert_eq!(dto.file_count, 1); // datas.md
        assert_eq!(dto.task_count, 0);
        assert_eq!(dto.overdue_count, 0);
        assert!(dto.created_at.len() >= 10);
        assert_eq!(dto.opened_at, None);

        let again = create_project(&conn, &wd, "example.ru");
        assert!(matches!(again, Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn invalid_project_names_are_rejected() {
        let (wd, conn) = setup("proj-names");
        for bad in ["", "   ", "a/b", "a\\b", "..", ".hidden", "a:b", "a*b"] {
            assert!(
                matches!(create_project(&conn, &wd, bad), Err(AppError::Invalid(_))),
                "accepted: {bad:?}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn rename_project_updates_folder_and_db_row() {
        let (wd, conn) = setup("proj-rename");
        let dto = create_project(&conn, &wd, "old").unwrap();
        let plan = rename_plan(&conn, &wd, dto.id, "new").unwrap();
        rename_move(&plan).unwrap();
        let renamed = dto_from_row(dto.id, rename_apply(&conn, dto.id, &plan).unwrap());
        assert_eq!(renamed.name, "new");
        assert!(!paths::projects_dir(&wd).join("old").exists());
        assert!(paths::projects_dir(&wd).join("new").is_dir());
        assert!(paths::projects_dir(&wd)
            .join("new")
            .join(paths::DATAS_MD)
            .is_file());
        let stored: String = conn
            .query_row("SELECT name FROM projects WHERE id = ?1", params![dto.id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "new");
        assert_eq!(project_path(&conn, &wd, dto.id).unwrap(), renamed.path);

        let clash = create_project(&conn, &wd, "taken").unwrap();
        assert!(matches!(
            rename_plan(&conn, &wd, dto.id, "taken"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            rename_plan(&conn, &wd, 999, "x"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
        let _ = clash;
    }

    // The staged flow the command uses: plan under the DB read, rename on
    // disk, apply the UPDATE. A conflicting DB state (row gone between plan
    // and apply) must fail with Db and the caller's rename_rollback must
    // restore the folder, as before.
    #[test]
    fn rename_rolls_back_folder_when_db_update_fails() {
        let (wd, conn) = setup("proj-rename-rollback");
        let dto = create_project(&conn, &wd, "old").unwrap();
        let plan = rename_plan(&conn, &wd, dto.id, "new").unwrap();
        rename_move(&plan).unwrap();
        assert!(!paths::projects_dir(&wd).join("old").exists());
        conn.execute("DELETE FROM projects WHERE id = ?1", params![dto.id])
            .unwrap();
        let err = rename_apply(&conn, dto.id, &plan).unwrap_err();
        assert!(matches!(err, AppError::Db(_)), "{err}");
        rename_rollback(&plan);
        let old_root = paths::projects_dir(&wd).join("old");
        assert!(old_root.is_dir());
        assert!(!paths::projects_dir(&wd).join("new").exists());
        assert!(old_root.join(paths::DATAS_MD).is_file());
        fs::remove_dir_all(&wd).unwrap();
    }

    // The staged create flow: the folder tree is made outside the DB, the
    // INSERT fails (conflicting DB state), and undo_create removes exactly
    // what this call created — the project folder, but not the pre-existing
    // Projects/ dir.
    #[test]
    fn create_project_cleans_up_created_folder_when_db_insert_fails() {
        let (wd, conn) = setup("proj-create-cleanup");
        let made = create_project_fs(&wd, "doomed").unwrap();
        let root = paths::projects_dir(&wd).join("doomed");
        assert!(root.join(paths::DATAS_MD).is_file());
        conn.execute("DROP TABLE projects", []).unwrap();
        let err = create_project_db(&conn, &made).unwrap_err();
        assert!(matches!(err, AppError::Db(_)), "{err}");
        undo_create(&made);
        assert!(!root.exists());
        assert!(paths::projects_dir(&wd).is_dir());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn list_projects_counts_tasks_and_overdue() {
        let (wd, conn) = setup("proj-list");
        let dto = create_project(&conn, &wd, "p1").unwrap();
        fs::write(paths::projects_dir(&wd).join("p1").join("a.txt"), "x").unwrap();
        fs::create_dir_all(paths::projects_dir(&wd).join("p1").join("sub/deep")).unwrap();
        fs::write(
            paths::projects_dir(&wd)
                .join("p1")
                .join("sub/deep/b.md"),
            "",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, status, due_date, created_at) VALUES
             (?1, 'past', 'new', date('now','-5 day'), '2026-01-01'),
             (?1, 'past-done', 'done', date('now','-5 day'), '2026-01-01'),
             (?1, 'future', 'progress', date('now','+5 day'), '2026-01-01'),
             (?1, 'no-due', 'new', NULL, '2026-01-01')",
            params![dto.id],
        )
        .unwrap();
        let list = list_projects(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].file_count, 3); // datas.md + a.txt + b.md
        assert_eq!(list[0].task_count, 4);
        assert_eq!(list[0].overdue_count, 1);
        assert!(list[0].has_active_tasks); // the 'future' task is in 'progress'
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn list_projects_has_active_tasks_tracks_progress() {
        let (wd, conn) = setup("proj-active");
        let p = create_project(&conn, &wd, "p").unwrap();
        // No tasks at all: no active indicator.
        assert!(!list_projects(&conn).unwrap()[0].has_active_tasks);

        // 'new' and 'done' tasks do not count as active.
        conn.execute(
            "INSERT INTO tasks (project_id, title, status, created_at) VALUES
             (?1, 'a', 'new', '2026-01-01'),
             (?1, 'b', 'done', '2026-01-01')",
            params![p.id],
        )
        .unwrap();
        assert!(!list_projects(&conn).unwrap()[0].has_active_tasks);

        // Transitioning a task into 'progress' turns the indicator on.
        let a: i64 = conn
            .query_row(
                "SELECT id FROM tasks WHERE project_id = ?1 AND title = 'a'",
                params![p.id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute("UPDATE tasks SET status = 'progress' WHERE id = ?1", params![a])
            .unwrap();
        assert!(list_projects(&conn).unwrap()[0].has_active_tasks);

        // Moving it back out of 'progress' turns it off again.
        conn.execute("UPDATE tasks SET status = 'done' WHERE id = ?1", params![a])
            .unwrap();
        assert!(!list_projects(&conn).unwrap()[0].has_active_tasks);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn touch_project_opened_sets_opened_at() {
        let (wd, conn) = setup("proj-touch");
        let dto = create_project(&conn, &wd, "p").unwrap();
        assert!(dto.opened_at.is_none());
        touch_project_opened(&conn, dto.id).unwrap();
        let opened: Option<String> = conn
            .query_row("SELECT opened_at FROM projects WHERE id = ?1", params![dto.id], |r| r.get(0))
            .unwrap();
        assert!(opened.is_some());
        assert!(matches!(touch_project_opened(&conn, 999), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    fn list_ids(conn: &Connection) -> Vec<i64> {
        list_projects(conn).unwrap().into_iter().map(|p| p.id).collect()
    }

    #[test]
    fn opening_a_project_does_not_reorder_and_create_appends() {
        let (wd, conn) = setup("proj-order");
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let p2 = create_project(&conn, &wd, "p2").unwrap();
        assert_eq!((p1.position, p2.position), (1.0, 2.0));
        touch_project_opened(&conn, p2.id).unwrap();
        touch_project_opened(&conn, p1.id).unwrap();
        let p3 = create_project(&conn, &wd, "p3").unwrap();
        assert_eq!(p3.position, 3.0);
        assert_eq!(list_ids(&conn), vec![p1.id, p2.id, p3.id]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn first_project_id_follows_list_order() {
        let (wd, conn) = setup("proj-first");
        assert_eq!(first_project_id(&conn).unwrap(), None);
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let p2 = create_project(&conn, &wd, "p2").unwrap();
        assert_eq!(first_project_id(&conn).unwrap(), Some(p1.id));
        move_project(&conn, p2.id, p1.position - 1.0).unwrap();
        assert_eq!(first_project_id(&conn).unwrap(), Some(p2.id));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn move_project_reorders_by_fractional_position() {
        let (wd, conn) = setup("proj-move");
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let p2 = create_project(&conn, &wd, "p2").unwrap();
        let p3 = create_project(&conn, &wd, "p3").unwrap();
        // drop p3 between p1 and p2 (midpoint of their positions)
        move_project(&conn, p3.id, (p1.position + p2.position) / 2.0).unwrap();
        assert_eq!(list_ids(&conn), vec![p1.id, p3.id, p2.id]);
        // move p2 to the top: one step before the current first
        move_project(&conn, p2.id, p1.position - 1.0).unwrap();
        assert_eq!(list_ids(&conn), vec![p2.id, p1.id, p3.id]);
        // move p2 to the end: one step after the current last
        let last = list_projects(&conn).unwrap().pop().unwrap();
        move_project(&conn, p2.id, last.position + 1.0).unwrap();
        assert_eq!(list_ids(&conn), vec![p1.id, p3.id, p2.id]);
        assert!(matches!(move_project(&conn, 999, 1.0), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_columns_default_and_are_per_project() {
        let (wd, conn) = setup("proj-task-columns");
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let p2 = create_project(&conn, &wd, "p2").unwrap();
        assert_eq!(p1.task_columns, ["Новые", "В работе", "Готово"]);
        assert_eq!(p2.task_columns, ["Новые", "В работе", "Готово"]);

        let renamed = set_task_columns(&conn, p1.id, vec![" Backlog ".into(), "Doing".into(), "Done".into()]).unwrap();
        assert_eq!(renamed.task_columns, ["Backlog", "Doing", "Done"]);
        // Per project: p2 keeps the defaults, and the stored JSON is trimmed.
        assert_eq!(list_projects(&conn).unwrap().iter().find(|p| p.id == p2.id).unwrap().task_columns,
                   ["Новые", "В работе", "Готово"]);
        let stored: String = conn
            .query_row("SELECT task_columns FROM projects WHERE id = ?1", params![p1.id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "[\"Backlog\",\"Doing\",\"Done\"]");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn set_task_columns_validates_names_and_project() {
        let (wd, conn) = setup("proj-task-columns-invalid");
        let p = create_project(&conn, &wd, "p").unwrap();
        for bad in [
            vec!["Новые".into(), "В работе".into()],
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            vec!["Новые".into(), "  ".into(), "Готово".into()],
            vec!["а".repeat(41), "b".into(), "c".into()],
        ] {
            assert!(
                matches!(set_task_columns(&conn, p.id, bad.clone()), Err(AppError::Invalid(_))),
                "accepted {bad:?}"
            );
        }
        assert!(matches!(
            set_task_columns(&conn, 999, vec!["a".into(), "b".into(), "c".into()]),
            Err(AppError::Invalid(_))
        ));
        // A 40-char name is at the cap and accepted.
        let ok = set_task_columns(&conn, p.id, vec!["а".repeat(40), "b".into(), "c".into()]).unwrap();
        assert_eq!(ok.task_columns[0].chars().count(), 40);
        // Corrupt stored JSON falls back to the defaults in the DTO.
        conn.execute("UPDATE projects SET task_columns = '[]' WHERE id = ?1", params![p.id])
            .unwrap();
        assert_eq!(get_project(&conn, p.id).unwrap().task_columns, ["Новые", "В работе", "Готово"]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn files_view_defaults_to_list_and_round_trips_per_project() {
        let (wd, conn) = setup("proj-files-view");
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let p2 = create_project(&conn, &wd, "p2").unwrap();
        assert_eq!(p1.files_view, "list");
        assert_eq!(p2.files_view, "list");

        let updated = set_files_view(&conn, p1.id, "grid").unwrap();
        assert_eq!(updated.files_view, "grid");
        // Per project: p2 keeps the default; the stored value is the mode itself.
        assert_eq!(list_projects(&conn).unwrap().iter().find(|p| p.id == p2.id).unwrap().files_view, "list");
        let stored: String = conn
            .query_row("SELECT files_view FROM projects WHERE id = ?1", params![p1.id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "grid");
        // Persistence: a fresh read (restart path) keeps the mode.
        assert_eq!(get_project(&conn, p1.id).unwrap().files_view, "grid");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn set_files_view_normalizes_unknown_values_and_validates_project() {
        let (wd, conn) = setup("proj-files-view-invalid");
        let p = create_project(&conn, &wd, "p").unwrap();
        // Case and whitespace are normalized; unknown values fall back to the default.
        assert_eq!(set_files_view(&conn, p.id, " Grid ").unwrap().files_view, "grid");
        assert_eq!(set_files_view(&conn, p.id, "bogus").unwrap().files_view, "list");
        assert_eq!(set_files_view(&conn, p.id, "").unwrap().files_view, "list");
        let stored: String = conn
            .query_row("SELECT files_view FROM projects WHERE id = ?1", params![p.id], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "list");
        assert!(matches!(set_files_view(&conn, 999, "grid"), Err(AppError::Invalid(_))));
        // DTO read fallback: any stored value outside the domain (the DB CHECK
        // normally prevents this) still renders as the default mode.
        assert_eq!(parse_files_view("tiles"), "list");
        assert_eq!(parse_files_view("grid"), "grid");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn archive_round_trip_stamps_and_clears_archived_at() {
        let (wd, conn) = setup("proj-archive");
        let p = create_project(&conn, &wd, "p").unwrap();
        assert!(!p.archived);
        assert_eq!(p.archived_at, None);

        let archived = set_archived(&conn, p.id, true).unwrap();
        assert!(archived.archived);
        assert!(archived.archived_at.as_deref().unwrap().len() >= 10);

        assert!(get_project(&conn, p.id).unwrap().archived);
        assert!(list_projects(&conn).unwrap()[0].archived);

        let restored = set_archived(&conn, p.id, false).unwrap();
        assert!(!restored.archived);
        assert_eq!(restored.archived_at, None);
        assert!(!get_project(&conn, p.id).unwrap().archived);

        assert!(matches!(set_archived(&conn, 999, true), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn list_flags_archived_keeps_order_and_retains_tasks() {
        let (wd, conn) = setup("proj-archive-list");
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let p2 = create_project(&conn, &wd, "p2").unwrap();
        let p3 = create_project(&conn, &wd, "p3").unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, status, created_at) VALUES (?1, 'kept', 'new', '2026-01-01')",
            params![p2.id],
        )
        .unwrap();

        set_archived(&conn, p2.id, true).unwrap();
        let list = list_projects(&conn).unwrap();
        assert_eq!(list.iter().map(|p| p.id).collect::<Vec<_>>(), vec![p1.id, p2.id, p3.id]);
        assert_eq!(
            list.iter().map(|p| p.archived).collect::<Vec<_>>(),
            vec![false, true, false]
        );
        let archived = list.iter().find(|p| p.id == p2.id).unwrap();
        assert!(archived.archived);
        assert_eq!(archived.task_count, 1);

        set_archived(&conn, p2.id, false).unwrap();
        assert_eq!(list_ids(&conn), vec![p1.id, p2.id, p3.id]);
        assert_eq!(get_project(&conn, p2.id).unwrap().task_count, 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn zero_vec_blob() -> Vec<u8> {
        vec![0u8; crate::embed::EMBED_DIM * 4]
    }

    #[test]
    fn delete_project_removes_everything_and_keeps_other_projects_intact() {
        let (wd, conn) = setup("proj-delete");
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let p1 = create_project(&conn, &wd, "gone").unwrap();
        let p2 = create_project(&conn, &wd, "kept").unwrap();
        let p1_root = paths::projects_dir(&wd).join("gone");

        // Task (syncs tasks_fts), project chat with message/summary/attachment.
        let t = tasks::task_create(&conn, p1.id, "редизайн главной", None).unwrap();
        conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at)
             VALUES (?1, 'project', datetime('now'), datetime('now'))",
            params![p1.id],
        )
        .unwrap();
        let chat_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position)
             VALUES (?1, 'user', 'привет', datetime('now'), 1)",
            params![chat_id],
        )
        .unwrap();
        let msg_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO summaries (chat_id, up_to_position, summary, created_at)
             VALUES (?1, 1, 'x', datetime('now'))",
            params![chat_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_attachments (message_id, name, saved_path, kind, is_image, created_at)
             VALUES (?1, 'a.png', 'a.png', 'image', 1, datetime('now'))",
            params![msg_id],
        )
        .unwrap();

        // Memory: project-scoped goes, global and other-project stay.
        let m1 = memory::add(&conn, "правки по редизайну", Some(p1.id), None, None).unwrap();
        let m2 = memory::add(&conn, "глобальная запись", None, None, None).unwrap();
        let m3 = memory::add(&conn, "память kept", Some(p2.id), None, None).unwrap();

        // Index legs: indexed_files, files_fts, file_vectors + vec_items.
        fs::write(p1_root.join("a.md"), "редизайн контент").unwrap();
        conn.execute(
            "INSERT INTO indexed_files (project_id, rel_path, name, kind, size, mtime, content_hash, indexed_at)
             VALUES (?1, 'a.md', 'a.md', 'text', 10, 1, NULL, datetime('now'))",
            params![p1.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files_fts (rel_path, project_id, name, content) VALUES ('a.md', ?1, 'a.md', 'редизайн')",
            params![p1.id.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (?1, ?2)",
            params![p1.id, zero_vec_blob()],
        )
        .unwrap();
        let vec_rowid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_vectors (project_id, rel_path, kind, dim, backend, vec_rowid, updated_at)
             VALUES (?1, 'a.md', 'text', 768, 'noop', ?2, datetime('now'))",
            params![p1.id, vec_rowid],
        )
        .unwrap();

        // Todos plan of the project chat scope.
        conn.execute(
            "INSERT INTO todos (scope_kind, scope_id, text, done, position, created_at, updated_at)
             VALUES ('project', ?1, 'сделать', 0, 1, datetime('now'), datetime('now'))",
            params![p1.id],
        )
        .unwrap();

        // Links on both sides of deleted entities.
        links::create(&conn, "project", &p1.id.to_string(), "task", &t.id.to_string()).unwrap();
        links::create(
            &conn,
            "file",
            &links::file_id(p1.id, "a.md"),
            "chat",
            &chat_id.to_string(),
        )
        .unwrap();
        links::create(&conn, "project", &p2.id.to_string(), "task", "999").unwrap();

        // Harness session with transcript mirrors and a schedule linked to the
        // project task: the session, its chat, messages and schedule all go.
        conn.execute(
            "INSERT INTO sessions (title, created_at, updated_at, last_active_at)
             VALUES ('s', datetime('now'), datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chats (kind, session_id, created_at, updated_at)
             VALUES ('harness', ?1, datetime('now'), datetime('now'))",
            params![sid],
        )
        .unwrap();
        let hchat = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position)
             VALUES (?1, 'user', 'редизайн в сессии', datetime('now'), 1)",
            params![hchat],
        )
        .unwrap();
        let hmsg = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages_fts (message_id, session_id, content) VALUES (?1, ?2, 'редизайн в сессии')",
            params![hmsg, sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_vectors (message_id, session_id, updated_at) VALUES (?1, ?2, datetime('now'))",
            params![hmsg, sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_messages (rowid, embedding) VALUES (?1, ?2)",
            params![hmsg, zero_vec_blob()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, status, related_task_id, created_at)
             VALUES (?1, 'run_at', '2026-01-01T10:00:00', 'active', ?2, datetime('now'))",
            params![sid, t.id],
        )
        .unwrap();
        // A schedule linked to the task but already detached from its session.
        conn.execute(
            "INSERT INTO schedules (kind, spec, status, related_task_id, created_at)
             VALUES ('cron', '0 10 * * *', 'active', ?1, datetime('now'))",
            params![t.id],
        )
        .unwrap();
        // An unrelated harness session must survive.
        conn.execute(
            "INSERT INTO sessions (title, created_at, updated_at, last_active_at)
             VALUES ('other', datetime('now'), datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        let other_sid = conn.last_insert_rowid();

        // Trash (project file + task snapshot), versions, undo entries. Paths
        // are canonicalized like production fs_ops records them.
        let p1_canon = p1_root.canonicalize().unwrap();
        let trash_file =
            reliability::trash_put(&conn, &wd, "project", "file", &p1_canon.join("a.md"), None).unwrap();
        let trash_task =
            reliability::trash_put_meta(&conn, "task", "task", &format!("task:{}", t.id), "t", "{}").unwrap();
        let other_trash =
            reliability::trash_put_meta(&conn, "task", "task", "task:999", "other", "{}").unwrap();
        let version =
            reliability::version_save(&conn, &wd, "project", &p1_canon.join("datas.md").display().to_string(), "old")
                .unwrap();
        conn.execute(
            "INSERT INTO undo_log (kind, payload, created_at)
             VALUES ('delete', json_object('trashId', ?1, 'projectId', ?2), datetime('now'))",
            params![trash_file, p1.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO undo_log (kind, payload, created_at)
             VALUES ('file_write', json_object('versionId', ?1, 'projectId', ?2), datetime('now'))",
            params![version, p1.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO undo_log (kind, payload, created_at)
             VALUES ('file_write', json_object('projectId', ?1), datetime('now'))",
            params![p2.id],
        )
        .unwrap();

        let plan = delete_project(&conn, &wd, p1.id).unwrap();
        delete_project_fs(&wd, &plan).unwrap();
        assert_eq!((plan.deleted_sessions, plan.deleted_schedules, plan.deleted_memory), (1, 2, 1));

        // Folder gone from Projects/, not moved to any Trash: the store root
        // and the other project are untouched.
        assert!(!p1_root.exists());
        assert!(paths::projects_dir(&wd).join("kept").is_dir());
        assert_eq!(count(&conn, "SELECT count(*) FROM projects"), 1);

        // Cascade-covered rows.
        assert_eq!(count(&conn, "SELECT count(*) FROM tasks WHERE project_id = 9999"), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM tasks WHERE project_id = {}", p1.id)), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM chats WHERE id = {chat_id}")), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM messages WHERE id IN ({msg_id}, {hmsg})")), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM summaries"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM message_attachments"), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM sessions WHERE id = {sid}")), 0);

        // Manually cleaned legs: no orphans anywhere.
        assert_eq!(count(&conn, "SELECT count(*) FROM tasks_fts"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM files_fts"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM indexed_files"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM file_vectors"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM vec_items"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM messages_fts"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM message_vectors"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM vec_messages"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM schedules"), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM todos WHERE scope_kind = 'project' AND scope_id = {}", p1.id)), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM links"), 1); // only the p2 link
        assert_eq!(count(&conn, "SELECT count(*) FROM undo_log"), 1); // only the p2 entry

        // Memory: project rows and their mirrors gone, global/other intact.
        assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 2);
        assert_eq!(
            count(&conn, &format!("SELECT count(*) FROM memory WHERE id IN ({}, {})", m2.id, m3.id)),
            2
        );
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM memory_fts WHERE rowid = {}", m1.id)), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM memory_vectors WHERE memory_id = {}", m1.id)), 0);
        assert_eq!(count(&conn, &format!("SELECT count(*) FROM vec_memory WHERE rowid = {}", m1.id)), 0);

        // Trash/versions: rows purged and stored snapshots removed from disk;
        // the unrelated trash row survives.
        assert_eq!(count(&conn, "SELECT count(*) FROM trash"), 1);
        assert_eq!(
            count(&conn, &format!("SELECT count(*) FROM trash WHERE id IN ({trash_file}, {trash_task})")),
            0
        );
        let stored_kept = conn
            .query_row("SELECT 1 FROM trash WHERE id = ?1", params![other_trash], |_| Ok(()))
            .is_ok();
        assert!(stored_kept);
        assert_eq!(count(&conn, "SELECT count(*) FROM versions"), 0);
        let versions_left: Vec<_> = fs::read_dir(paths::versions_dir(&wd)).unwrap().collect();
        assert!(versions_left.is_empty());

        // The deleted project is invisible to search; the kept one still works.
        let hits = crate::search::search_with(
            &conn,
            &crate::embed::NoopEmbedder,
            crate::embed::NAME_WEIGHT,
            p1.id,
            "редизайн",
            10,
        )
        .unwrap();
        assert!(hits.is_empty());
        assert_eq!(
            count(&conn, &format!("SELECT count(*) FROM sessions WHERE id = {other_sid}")),
            1
        );

        // Deleting an unknown project errors without touching anything.
        assert!(matches!(delete_project(&conn, &wd, 9999), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_project_refuses_paths_outside_the_projects_root() {
        let (wd, conn) = setup("proj-delete-safety");
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let projects = paths::projects_dir(&wd);

        let insert_row = |name: &str, path: &str| -> i64 {
            conn.execute(
                "INSERT INTO projects (name, path, created_at) VALUES (?1, ?2, datetime('now'))",
                params![name, path],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        // The working dir itself, the Projects root, and "/" are refused.
        for path in [wd.display().to_string(), projects.display().to_string(), "/".into()] {
            let id = insert_row("bad", &path);
            assert!(
                matches!(delete_project(&conn, &wd, id), Err(AppError::Invalid(_))),
                "accepted {path}"
            );
        }
        assert!(wd.is_dir() && projects.is_dir());

        // Traversal outside Projects/ is refused (folder present and absent).
        let outside = wd.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "x").unwrap();
        let id = insert_row("trav", projects.join("../outside").display().to_string().as_str());
        assert!(matches!(delete_project(&conn, &wd, id), Err(AppError::Invalid(_))));
        let id = insert_row("trav2", projects.join("../nope").display().to_string().as_str());
        assert!(matches!(delete_project(&conn, &wd, id), Err(AppError::Invalid(_))));
        assert!(outside.join("keep.txt").is_file());

        // A symlink inside Projects/ pointing elsewhere is refused and kept.
        std::os::unix::fs::symlink(&outside, projects.join("evil")).unwrap();
        let id = insert_row("evil", projects.join("evil").display().to_string().as_str());
        assert!(matches!(delete_project(&conn, &wd, id), Err(AppError::Invalid(_))));
        assert!(outside.join("keep.txt").is_file());
        assert!(projects.join("evil").is_symlink());

        // A real folder that is not a direct child of Projects/ is refused.
        let nested = projects.join("outer/inner");
        fs::create_dir_all(&nested).unwrap();
        let id = insert_row("nested", nested.display().to_string().as_str());
        assert!(matches!(delete_project(&conn, &wd, id), Err(AppError::Invalid(_))));
        assert!(nested.is_dir());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_project_cleans_db_when_folder_is_already_gone() {
        let (wd, conn) = setup("proj-delete-missing-folder");
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let p = create_project(&conn, &wd, "ghost").unwrap();
        fs::remove_dir_all(paths::projects_dir(&wd).join("ghost")).unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, status, created_at) VALUES (?1, 't', 'new', '2026-01-01')",
            params![p.id],
        )
        .unwrap();
        let plan = delete_project(&conn, &wd, p.id).unwrap();
        delete_project_fs(&wd, &plan).unwrap();
        assert_eq!(count(&conn, "SELECT count(*) FROM projects"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM tasks"), 0);
        fs::remove_dir_all(&wd).unwrap();
    }
}
