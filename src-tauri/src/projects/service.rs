// Project business rules: name and path validation, the per-project settings
// (kanban columns, file view, archive flag), the builtin task inbox and the
// on-disk halves of the create/rename lifecycle. SQL/data access lives in
// `repo`, DTO/plan types in `types`; the public surface is re-exported from
// the parent module.
use crate::error::{AppError, Result};
use crate::paths;
use crate::projects::repo::{
    builtin_project_id, create_project_db, db_row, dto, dto_from_row, is_builtin,
};
use crate::projects::types::{
    normalize_files_view, CreatedProject, ProjectDbRow, ProjectDto, RenamePlan,
    BUILTIN_PROJECT_DIR, BUILTIN_PROJECT_NAME, MAX_TASK_COLUMN_NAME_CHARS,
};
use rusqlite::{params, Connection};
use std::fs;
use std::path::{Path, PathBuf};

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
    if archived && is_builtin(conn, id)? {
        return Err(AppError::Invalid(
            "the builtin project cannot be archived".into(),
        ));
    }
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

// Get-or-create the permanent builtin project. Existing installs reach it on
// the first startup after the migration; repeated calls return the same row.
// The hidden folder is recreated if it went missing, and the chat is ensured
// so the static greeting is seeded exactly once.
pub fn ensure_builtin_project(conn: &Connection, wd: &Path) -> Result<i64> {
    let projects = paths::projects_dir(wd);
    fs::create_dir_all(&projects)?;
    let root = projects.join(BUILTIN_PROJECT_DIR);
    fs::create_dir_all(&root)?;
    let id = match builtin_project_id(conn)? {
        Some(id) => id,
        None => {
            conn.execute(
                "INSERT INTO projects (name, path, created_at, position, builtin)
                 VALUES (?1, ?2, datetime('now','localtime'), 0, 1)",
                params![BUILTIN_PROJECT_NAME, root.display().to_string()],
            )?;
            conn.last_insert_rowid()
        }
    };
    crate::chat::ensure_project_chat(conn, id)?;
    Ok(id)
}

// Renaming the builtin project changes only the display name: the hidden
// service folder and the builtin flag stay put.
pub fn rename_builtin(conn: &Connection, id: i64, raw_name: &str) -> Result<ProjectDbRow> {
    let name = sanitize_project_name(raw_name)?;
    let updated = conn.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2 AND builtin = 1",
        params![name, id],
    )?;
    if updated != 1 {
        return Err(AppError::Invalid(format!("project {id} not found")));
    }
    db_row(conn, id)
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

pub(crate) fn rename_rollback(plan: &RenamePlan) {
    let _ = fs::rename(&plan.new_root, &plan.old_root);
}
