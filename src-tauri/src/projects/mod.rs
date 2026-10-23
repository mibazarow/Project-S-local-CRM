// Projects domain (SPEC 4.1): the project list, the folder lifecycle (create /
// rename / archive / delete), the per-project settings (kanban columns, file
// view) and the builtin task inbox. Split per R-11: `repo` holds the SQL/data
// access (row reads and writes, the DTO assembly, the delete transaction and
// its cleanup plumbing), `service` holds the business rules, name/path
// validation and the on-disk lifecycle steps, `types` holds the DTO/row/plan
// types and the domain constants. The public surface is re-exported here so
// call sites keep the `projects::…` paths.
mod repo;
mod service;
mod types;

pub use repo::{
    delete_project, delete_project_fs, first_project_id, get_project, is_builtin, list_projects,
    move_project, touch_project_opened,
};
pub(crate) use repo::{create_project_db, dto_from_row, list_project_rows, rename_apply};
pub use service::{
    create_project, ensure_builtin_project, project_path, rename_builtin, sanitize_project_name,
    set_archived, set_files_view, set_task_columns,
};
pub(crate) use service::{
    create_project_fs, project_root, rename_move, rename_plan, rename_rollback, undo_create,
};
pub use types::ProjectDto;
pub(crate) use types::BUILTIN_PROJECT_GREETING;
// Test convenience: other domains' test fixtures build a ProjectDto by hand.
#[cfg(test)]
pub(crate) use types::{DEFAULT_FILES_VIEW, DEFAULT_TASK_COLUMNS};

#[cfg(test)]
mod tests {
    use super::types::{parse_files_view, BUILTIN_PROJECT_DIR, BUILTIN_PROJECT_NAME};
    use super::*;
    use crate::db::schema;
    use crate::error::AppError;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::{links, memory, reliability, tasks};
    use rusqlite::{params, Connection};
    use std::fs;
    use std::path::PathBuf;

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

    #[test]
    fn builtin_project_is_created_once_and_sorts_first() {
        let (wd, conn) = setup("proj-builtin");
        let p1 = create_project(&conn, &wd, "p1").unwrap();
        let id = ensure_builtin_project(&conn, &wd).unwrap();
        assert_eq!(ensure_builtin_project(&conn, &wd).unwrap(), id);

        let list = list_projects(&conn).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, id);
        assert!(list[0].builtin);
        assert_eq!(list[0].name, BUILTIN_PROJECT_NAME);
        assert!(!list[1].builtin);
        assert_eq!(list[1].id, p1.id);
        assert!(paths::projects_dir(&wd).join(BUILTIN_PROJECT_DIR).is_dir());
        assert_eq!(first_project_id(&conn).unwrap(), Some(id));

        move_project(&conn, p1.id, -100.0).unwrap();
        assert_eq!(list_ids(&conn), vec![id, p1.id]);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn builtin_project_is_separate_from_a_same_named_user_project() {
        let (wd, conn) = setup("proj-builtin-name");
        let named = create_project(&conn, &wd, BUILTIN_PROJECT_NAME).unwrap();
        let builtin = ensure_builtin_project(&conn, &wd).unwrap();
        assert_ne!(named.id, builtin);
        assert!(!named.builtin);
        assert!(is_builtin(&conn, builtin).unwrap());
        assert_eq!(count(&conn, "SELECT count(*) FROM projects"), 2);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn builtin_project_cannot_be_archived_or_deleted() {
        let (wd, conn) = setup("proj-builtin-protected");
        let id = ensure_builtin_project(&conn, &wd).unwrap();
        assert!(matches!(set_archived(&conn, id, true), Err(AppError::Invalid(_))));
        assert!(matches!(delete_project(&conn, &wd, id), Err(AppError::Invalid(_))));
        let dto = list_projects(&conn).unwrap().into_iter().find(|p| p.id == id).unwrap();
        assert!(!dto.archived);
        assert!(is_builtin(&conn, id).unwrap());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn builtin_chat_greets_once_and_plain_projects_do_not() {
        let (wd, conn) = setup("proj-builtin-greeting");
        let id = ensure_builtin_project(&conn, &wd).unwrap();
        let chat_id = crate::chat::ensure_project_chat(&conn, id).unwrap();
        let messages = |chat_id: i64| -> Vec<(String, Option<String>)> {
            let mut stmt = conn
                .prepare("SELECT role, content FROM messages WHERE chat_id = ?1 ORDER BY position")
                .unwrap();
            let rows = stmt.query_map(params![chat_id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        let first = messages(chat_id);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "assistant");
        assert_eq!(first[0].1.as_deref(), Some(BUILTIN_PROJECT_GREETING));

        assert_eq!(crate::chat::ensure_project_chat(&conn, id).unwrap(), chat_id);
        assert_eq!(messages(chat_id).len(), 1);

        let plain = create_project(&conn, &wd, "p").unwrap();
        let plain_chat = crate::chat::ensure_project_chat(&conn, plain.id).unwrap();
        assert!(messages(plain_chat).is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn builtin_rename_keeps_flag_and_hidden_folder() {
        let (wd, conn) = setup("proj-builtin-rename");
        let id = ensure_builtin_project(&conn, &wd).unwrap();
        let renamed = rename_builtin(&conn, id, "Личное").unwrap();
        assert_eq!(renamed.name, "Личное");
        let dto = list_projects(&conn).unwrap().into_iter().find(|p| p.id == id).unwrap();
        assert_eq!(dto.name, "Личное");
        assert!(dto.builtin);
        assert_eq!(
            dto.path,
            paths::projects_dir(&wd).join(BUILTIN_PROJECT_DIR).display().to_string()
        );
        assert!(paths::projects_dir(&wd).join(BUILTIN_PROJECT_DIR).is_dir());
        assert!(!paths::projects_dir(&wd).join("Личное").exists());
        assert!(matches!(rename_builtin(&conn, 999, "x"), Err(AppError::Invalid(_))));
        fs::remove_dir_all(&wd).unwrap();
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }    fn zero_vec_blob() -> Vec<u8> {
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

    #[cfg(unix)]
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

    // Pre-R-15 reference shape of the list: one id query plus three queries per
    // project, with the original standalone aggregate SQL.
    const TASK_COUNT_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1";
    const OVERDUE_SQL: &str = "SELECT count(*) FROM tasks WHERE project_id = ?1 AND status != 'done' AND due_date IS NOT NULL AND due_date || ' ' || due_time || ':00' < datetime('now','localtime')";

    fn list_project_rows_rowwise(conn: &Connection) -> Vec<(i64, types::ProjectDbRow)> {
        let ids = conn
            .prepare("SELECT id FROM projects ORDER BY builtin DESC, position, id")
            .unwrap()
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        ids.into_iter()
            .map(|id| {
                let mut row = repo::db_row(conn, id).unwrap();
                row.task_count = conn.query_row(TASK_COUNT_SQL, params![id], |r| r.get(0)).unwrap();
                row.overdue_count = conn.query_row(OVERDUE_SQL, params![id], |r| r.get(0)).unwrap();
                (id, row)
            })
            .collect()
    }

    // Fixture for the R-15 list rewrite: builtin first, a position tie broken
    // by id, a negative position, a project without tasks, and the overdue
    // edge cases (done task with a past due date, task without a due date).
    fn seed_list_fixture(conn: &Connection) {
        conn.execute(
            "INSERT INTO projects (id, name, path, created_at, opened_at, position, archived_at, builtin)
             VALUES (1, 'builtin', '/tmp/builtin', '2026-01-01', NULL, 0, NULL, 1),
                    (2, 'arch', '/tmp/arch', '2026-01-01', '2026-02-01', 5.5, '2026-03-01', 0),
                    (3, 'late', '/tmp/late', '2026-01-01', NULL, 5.5, NULL, 0),
                    (4, 'empty', '/tmp/empty', '2026-01-01', NULL, -1, NULL, 0)",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (project_id, title, status, due_date, due_time, created_at) VALUES
                (1, 'a', 'progress', '2000-01-01', '09:00', '2026-01-01'),
                (1, 'b', 'done', '2000-01-01', '09:00', '2026-01-01'),
                (1, 'c', 'new', NULL, '07:00', '2026-01-01'),
                (2, 'd', 'done', '2000-01-01', '07:00', '2026-01-01'),
                (2, 'e', 'done', '2000-01-01', '07:00', '2026-01-01'),
                (3, 'f', 'new', '2000-01-01', '07:00', '2026-01-01'),
                (3, 'g', 'new', '2000-01-02', '23:59', '2026-01-01'),
                (3, 'h', 'progress', NULL, '07:00', '2026-01-01');",
        )
        .unwrap();
    }

    #[test]
    fn project_list_single_query_matches_the_rowwise_reference() {
        let (wd, conn) = setup("proj-list-equiv");
        seed_list_fixture(&conn);
        let rowwise = list_project_rows_rowwise(&conn);
        let single = list_project_rows(&conn).unwrap();
        assert_eq!(rowwise, single);
        // Order and aggregates pinned independently of the reference: builtin
        // first, then position, then id; overdue skips done and due-less tasks.
        assert_eq!(
            single.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 4, 2, 3]
        );
        let aggregates = single
            .iter()
            .map(|(_, r)| (r.task_count, r.overdue_count, r.has_active_tasks))
            .collect::<Vec<_>>();
        assert_eq!(
            aggregates,
            vec![(3, 1, true), (0, 0, false), (2, 0, false), (3, 2, true)]
        );

        let (wd2, conn2) = setup("proj-list-empty");
        assert!(list_project_rows_rowwise(&conn2).is_empty());
        assert!(list_project_rows(&conn2).unwrap().is_empty());
        assert!(matches!(
            repo::db_row(&conn2, 1),
            Err(AppError::Db(_))
        ));
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&wd2).unwrap();
    }

    #[test]
    fn project_list_is_a_single_query_regardless_of_project_count() {
        use rusqlite::trace::{TraceEvent, TraceEventCodes};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static QUERIES: AtomicUsize = AtomicUsize::new(0);
        fn count(ev: TraceEvent<'_>) {
            if matches!(ev, TraceEvent::Stmt(..)) {
                QUERIES.fetch_add(1, Ordering::Relaxed);
            }
        }
        let (wd, conn) = setup("proj-list-queries");
        seed_list_fixture(&conn);
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count));
        QUERIES.store(0, Ordering::Relaxed);
        assert_eq!(list_project_rows(&conn).unwrap().len(), 4);
        assert_eq!(QUERIES.swap(0, Ordering::Relaxed), 1);
        assert_eq!(repo::db_row(&conn, 1).unwrap().task_count, 3);
        assert_eq!(QUERIES.swap(0, Ordering::Relaxed), 1);
        conn.trace_v2(TraceEventCodes::empty(), None);
        fs::remove_dir_all(&wd).unwrap();
    }

}
