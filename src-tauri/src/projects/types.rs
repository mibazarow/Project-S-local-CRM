// Project DTOs, the DB row shape behind them, the staged-operation plan types
// and the domain constants. SQL/data access lives in `repo`, business rules
// and validation in `service`; the public surface is re-exported from the
// parent module.
use serde::Serialize;
use std::path::PathBuf;

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
    pub builtin: bool,
}

// Per-project kanban column names (v31): a JSON array of exactly 3 strings.
pub const DEFAULT_TASK_COLUMNS: [&str; 3] = ["Новые", "В работе", "Готово"];
pub const MAX_TASK_COLUMN_NAME_CHARS: usize = 40;

// Permanent builtin project (Q.md item 2): a personal task inbox that always
// exists, always sorts first, is never archived or deleted, and keeps its
// chat attachments in a hidden service folder under Projects/.
pub const BUILTIN_PROJECT_NAME: &str = "Мои задачи";
pub const BUILTIN_PROJECT_DIR: &str = ".my-tasks";
pub const BUILTIN_PROJECT_GREETING: &str = "Это ваш личный задачник: здесь можно ставить задачи, не привязанные ни к какому проекту — разовые дела, напоминания, быстрые поручения. Всё работает как в обычном проекте: колонки, сроки, приоритеты. Создайте задачу или просто напишите, что нужно сделать.";

pub(crate) fn parse_task_columns(raw: &str) -> [String; 3] {
    serde_json::from_str::<Vec<String>>(raw)
        .ok()
        .filter(|v| v.len() == 3)
        .map(|v| [v[0].clone(), v[1].clone(), v[2].clone()])
        .unwrap_or_else(|| DEFAULT_TASK_COLUMNS.map(str::to_owned))
}

// Per-project file view mode (SPEC 4.2): "list" | "grid". Unknown values
// normalize to the default on write, and corrupt stored values fall back to
// it on read, so the DTO always carries a valid mode.
pub const DEFAULT_FILES_VIEW: &str = "list";

pub(crate) fn parse_files_view(raw: &str) -> String {
    if raw == "grid" {
        "grid".to_owned()
    } else {
        DEFAULT_FILES_VIEW.to_owned()
    }
}

pub(crate) fn normalize_files_view(raw: &str) -> String {
    parse_files_view(&raw.trim().to_ascii_lowercase())
}

// Permanent project deletion (Q.md item 1): no Trash, no undo path. The DB
// cleanup runs in one transaction (delete_project); the filesystem cleanup
// (delete_project_fs) runs only after the commit, so a failed folder removal
// never resurrects the DB record.

// What delete_project collected for the post-commit filesystem cleanup.
#[derive(Debug)]
pub struct DeletePlan {
    // Canonicalized project folder to remove; None when it is already gone.
    pub(crate) root: Option<PathBuf>,
    // stored_path of the purged trash rows (the <id> store dir is removed).
    pub(crate) trash_stored: Vec<PathBuf>,
    // stored_path of the purged version snapshots.
    pub(crate) version_stored: Vec<PathBuf>,
    pub deleted_sessions: usize,
    pub deleted_schedules: usize,
    pub deleted_memory: usize,
}

// Everything the create flow does on disk, with no DB touched: the command
// runs this outside the DB mutex, then calls create_project_db for the single
// INSERT. `undo_create` removes only what this call created.
#[derive(Debug)]
pub(crate) struct CreatedProject {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) projects_dir: PathBuf,
    pub(crate) created_projects_dir: bool,
}

// Everything rename_project does except the folder rename and the tree walk:
// the command resolves under a short DB read, renames outside the DB mutex,
// then applies the single UPDATE. On DB failure the caller renames back with
// rename_rollback (same compensation as before).
#[derive(Debug)]
pub(crate) struct RenamePlan {
    pub(crate) name: String,
    pub(crate) old_root: PathBuf,
    pub(crate) new_root: PathBuf,
}

// Everything a project DTO needs from the DB, without the filesystem walk.
#[derive(Debug)]
pub(crate) struct ProjectDbRow {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) created_at: String,
    pub(crate) opened_at: Option<String>,
    pub(crate) position: f64,
    pub(crate) task_columns: String,
    pub(crate) files_view: String,
    pub(crate) archived_at: Option<String>,
    pub(crate) builtin: bool,
    pub(crate) task_count: i64,
    pub(crate) overdue_count: i64,
    pub(crate) has_active_tasks: bool,
}
