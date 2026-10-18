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
    pub task_columns: [String; 3],
}

const TASK_COUNT_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1";
const OVERDUE_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1 AND status != 'done' AND due_date IS NOT NULL AND due_date < date('now','localtime')";

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

pub fn list_projects(conn: &Connection) -> Result<Vec<ProjectDto>> {
    let mut ids = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY position, id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        for row in rows {
            ids.push(row?);
        }
    }
    ids.iter().map(|id| dto(conn, *id)).collect()
}

pub fn create_project(conn: &Connection, wd: &Path, raw_name: &str) -> Result<ProjectDto> {
    let name = sanitize_project_name(raw_name)?;
    let projects = paths::projects_dir(wd);
    fs::create_dir_all(&projects)?;
    let root = projects.join(&name);
    if root.exists() {
        return Err(AppError::Invalid(format!("project \"{name}\" already exists")));
    }
    fs::create_dir_all(root.join(paths::PROJECT_TASKS))?;
    fs::create_dir_all(root.join(paths::PROJECT_INBOX))?;
    fs::write(root.join(paths::DATAS_MD), "")?;
    conn.execute(
        "INSERT INTO projects (name, path, created_at, position)
         VALUES (?1, ?2, datetime('now','localtime'),
                 (SELECT COALESCE(MAX(position), 0) + 1 FROM projects))",
        params![name, root.display().to_string()],
    )?;
    dto(conn, conn.last_insert_rowid())
}

// Renames the folder on disk first, then updates the DB row in one statement.
pub fn rename_project(conn: &Connection, wd: &Path, id: i64, raw_name: &str) -> Result<ProjectDto> {
    let name = sanitize_project_name(raw_name)?;
    let old_root = project_root(conn, wd, id)?;
    let new_root = paths::projects_dir(wd).canonicalize()?.join(&name);
    if new_root.exists() {
        return Err(AppError::Invalid(format!("project \"{name}\" already exists")));
    }
    fs::rename(&old_root, &new_root)?;
    let updated = conn.execute(
        "UPDATE projects SET name = ?1, path = ?2 WHERE id = ?3",
        params![name, new_root.display().to_string(), id],
    )?;
    if updated != 1 {
        let _ = fs::rename(&new_root, &old_root);
        return Err(AppError::Db(format!("project {id} not found")));
    }
    dto(conn, id)
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

fn dto(conn: &Connection, id: i64) -> Result<ProjectDto> {
    let (name, path, created_at, opened_at, position, task_columns) = conn.query_row(
        "SELECT name, path, created_at, opened_at, position, task_columns FROM projects WHERE id = ?1",
        params![id],
        |r| Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, String>(5)?,
        )),
    )?;
    let task_count = conn.query_row(TASK_COUNT_SQL, params![id], |r| r.get(0))?;
    let overdue_count = conn.query_row(OVERDUE_SQL, params![id], |r| r.get(0))?;
    Ok(ProjectDto {
        id,
        name,
        path: path.clone(),
        created_at,
        opened_at,
        position,
        file_count: count_files(Path::new(&path)),
        task_count,
        overdue_count,
        task_columns: parse_task_columns(&task_columns),
    })
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
        let renamed = rename_project(&conn, &wd, dto.id, "new").unwrap();
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
            rename_project(&conn, &wd, dto.id, "taken"),
            Err(AppError::Invalid(_))
        ));
        assert!(matches!(
            rename_project(&conn, &wd, 999, "x"),
            Err(AppError::Invalid(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
        let _ = clash;
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
}
