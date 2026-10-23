// Tasks domain (SPEC 4.3, 4.4): the kanban board of a project — CRUD, the
// wall-clock timer, overdue derivation, the trash snapshot/restore plumbing
// and the tasks_fts mirror. Split per R-11: `repo` holds the SQL/data access
// (row reads and writes, FTS sync, snapshot plumbing, clock helpers),
// `service` holds the business rules and validation behind the public
// operations, `types` holds the DTO/patch/snapshot types and constants. The
// public surface is re-exported here so call sites keep the `tasks::…` paths.
mod repo;
mod service;
mod types;

pub use repo::tasks_list;
pub(crate) use repo::{task_delete_row, task_restore_snapshot, task_snapshot};
pub use service::{task_create_with, task_update};
pub(crate) use service::{task_delete_with_trash, task_move_capture, task_restore_from_trash};
// Test convenience wrappers over the *_with/_capture/_with_trash variants;
// other domains' tests call them through this module.
#[cfg(test)]
pub use service::{task_create, task_delete, task_move};
#[cfg(test)]
pub use types::MAX_CHECKLIST_ITEMS;
pub use types::{ChecklistItem, TaskDto, TaskPatch, STATUSES};
pub(crate) use types::TaskSnapshot;

#[cfg(test)]
mod tests {
    use super::repo::{dto, is_overdue};
    use super::*;
    use crate::db::schema;
    use crate::error::AppError;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::reliability;
    use rusqlite::{params, Connection};
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
                is_priority: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(updated.title, "renamed");
        assert_eq!(updated.comment.as_deref(), Some("note"));
        assert_eq!(updated.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(updated.remind_at.as_deref(), Some("2099-01-02T10:30"));
        assert!(updated.is_priority);

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
    fn task_create_is_rejected_for_archived_projects() {
        let (wd, conn, pid) = setup("tasks-archived");
        let before = task_create(&conn, pid, "before", None).unwrap();
        projects::set_archived(&conn, pid, true).unwrap();

        let err = task_create(&conn, pid, "blocked", None).unwrap_err();
        match err {
            AppError::Invalid(msg) => assert!(msg.contains("архив"), "{msg}"),
            other => panic!("{other}"),
        }

        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.iter().map(|t| t.id).collect::<Vec<_>>(), vec![before.id]);

        projects::set_archived(&conn, pid, false).unwrap();
        task_create(&conn, pid, "after", None).unwrap();
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 2);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_leaves_a_task_trash_row_with_the_full_snapshot() {
        let (wd, conn, pid) = setup("tasks-trash-shape");
        let t = task_create_with(
            &conn,
            pid,
            "отчёт",
            Some("progress".into()),
            Some("2099-01-02".into()),
            None,
            Some(true),
            Some(vec![ChecklistItem { text: "send".into(), done: false }]),
        )
        .unwrap();
        conn.execute("UPDATE tasks SET recurrence = 'weekly' WHERE id = ?1", params![t.id]).unwrap();
        let (project_id, trash_id) = task_delete_with_trash(&conn, t.id).unwrap();
        assert_eq!(project_id, pid);
        let row: (String, String, String, String, String, String, i64) = conn
            .query_row(
                "SELECT scope, kind, original_path, stored_path, name, meta, id
                 FROM trash WHERE id = ?1",
                params![trash_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        assert_eq!(row.0, "task");
        assert_eq!(row.1, "task");
        assert_eq!(row.2, format!("task:{}", t.id));
        assert_eq!(row.3, "", "a task trash row keeps no file on disk");
        assert_eq!(row.4, "отчёт");
        assert_eq!(row.6, trash_id);
        let meta: serde_json::Value = serde_json::from_str(&row.5).unwrap();
        assert_eq!(meta["id"], t.id);
        assert_eq!(meta["projectId"], pid);
        assert_eq!(meta["status"], "progress");
        assert_eq!(meta["dueDate"], "2099-01-02");
        assert_eq!(meta["dueTime"], "07:00");
        assert_eq!(meta["isPriority"], true);
        assert_eq!(meta["recurrence"], "weekly");
        assert_eq!(meta["checklist"], "[{\"text\":\"send\",\"done\":false}]");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_of_a_missing_task_errors_without_touching_rows_or_trash() {
        let (wd, conn, pid) = setup("tasks-delete-rollback");
        let t = task_create(&conn, pid, "keep", None).unwrap();
        assert!(matches!(task_delete_with_trash(&conn, 9999), Err(AppError::Invalid(_))));
        assert_eq!(tasks_list(&conn, pid).unwrap().len(), 1);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM trash", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "a failed delete must not leave a trash row"
        );
        assert_eq!(task_snapshot(&conn, t.id).unwrap().title, "keep");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trash_restore_reinserts_the_same_id_and_reindexes_fts() {
        let (wd, conn, pid) = setup("tasks-trash-restore");
        let t = task_create(&conn, pid, "восстановить", None).unwrap();
        let (_, trash_id) = task_delete_with_trash(&conn, t.id).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM tasks_fts WHERE tasks_fts MATCH ?1",
                params!["восстановить"],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "delete must clear the fts mirror"
        );
        let row = reliability::TrashDto {
            id: trash_id,
            scope: "task".into(),
            kind: "task".into(),
            original_path: format!("task:{}", t.id),
            stored_path: String::new(),
            name: "восстановить".into(),
            deleted_at: String::new(),
            meta: conn
                .query_row("SELECT meta FROM trash WHERE id = ?1", params![trash_id], |r| r.get(0))
                .unwrap(),
            project_id: None,
        };
        let restored_pid = task_restore_from_trash(&conn, &row).unwrap();
        assert_eq!(restored_pid, pid);
        let list = tasks_list(&conn, pid).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, t.id, "restore must reuse the original id");
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM tasks_fts WHERE tasks_fts MATCH ?1",
                params!["восстановить"],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "restore must reindex the fts mirror"
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn dto_json_carries_is_priority_and_drops_tags_and_priority() {
        let (wd, conn, pid) = setup("tasks-dto-json");
        let t = task_create_with(
            &conn,
            pid,
            "t",
            None,
            Some("2099-01-02".into()),
            None,
            Some(true),
            None,
        )
        .unwrap();
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["isPriority"], true);
        assert_eq!(json["dueDate"], "2099-01-02");
        assert!(json.get("tags").is_none(), "tags must be gone from the DTO");
        assert!(json.get("priority").is_none(), "priority must be gone from the DTO");
        // Stale priority/tags keys an old caller may still send are ignored.
        let patch: TaskPatch = serde_json::from_str(r#"{"priority": "urgent", "tags": ["x"]}"#).unwrap();
        let kept = task_update(&conn, t.id, patch).unwrap();
        assert!(kept.is_priority, "stale keys must not flip the flag");
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
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fts_count("старое"), 0, "old text must be gone after update");
        assert_eq!(fts_count("новое"), 1);
        assert_eq!(fts_count("комментарий"), 1);

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
    fn timer_start_stop_resume_accumulates() {
        let (wd, conn, pid) = setup("tasks-timer-accumulate");
        let t = task_create(&conn, pid, "t", None).unwrap();
        assert_eq!(t.spent_ms, 0);
        assert_eq!(t.started_at, None, "a 'new' task has no active start");

        // Entering 'progress' starts the clock.
        let started = task_move(&conn, t.id, "progress", 1.0).unwrap();
        assert!(started.started_at.is_some());
        assert_eq!(started.spent_ms, 0);

        // Back to 'new' stops it and banks the elapsed time.
        let stopped = task_move(&conn, t.id, "new", 1.0).unwrap();
        assert_eq!(stopped.started_at, None);
        assert!(stopped.spent_ms >= 0);

        // Returning to 'progress' resumes: the stamp is fresh, the banked
        // time is kept (not reset, not double-counted).
        let resumed = task_move(&conn, t.id, "progress", 1.0).unwrap();
        assert!(resumed.started_at.is_some());
        assert_eq!(resumed.spent_ms, stopped.spent_ms);

        // 'done' stops it too; the total survives.
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert_eq!(done.started_at, None);
        assert!(done.spent_ms >= stopped.spent_ms);

        // A task created straight into 'progress' starts counting at once.
        let born_running = task_create(&conn, pid, "born", Some("progress".into())).unwrap();
        assert!(born_running.started_at.is_some());
        assert_eq!(born_running.spent_ms, 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_wall_clock_accumulates_across_a_simulated_gap() {
        let (wd, conn, pid) = setup("tasks-timer-wallclock");
        let t = task_create(&conn, pid, "t", None).unwrap();
        task_move(&conn, t.id, "progress", 1.0).unwrap();
        // Simulate the app having been closed for two hours: rewind the
        // start stamp; the next stop must bank the whole wall-clock span.
        conn.execute(
            "UPDATE tasks SET started_at = started_at - 7200000 WHERE id = ?1",
            params![t.id],
        )
        .unwrap();
        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        assert!(
            done.spent_ms >= 7_200_000 && done.spent_ms < 7_205_000,
            "two simulated hours must land in spent_ms, got {}",
            done.spent_ms
        );
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_no_double_count_on_same_status_moves() {
        let (wd, conn, pid) = setup("tasks-timer-no-double");
        let t = task_create(&conn, pid, "t", None).unwrap();
        let started = task_move(&conn, t.id, "progress", 1.0).unwrap();
        let stamp = started.started_at.unwrap();

        // Reordering inside the column (same status) must not restart or
        // bank anything.
        let reordered = task_move(&conn, t.id, "progress", 9.0).unwrap();
        assert_eq!(reordered.started_at, Some(stamp), "same-status move must keep the stamp");
        assert_eq!(reordered.spent_ms, 0);

        let done = task_move(&conn, t.id, "done", 1.0).unwrap();
        let total = done.spent_ms;
        assert!(total >= 0);
        // Repeating the done move must not bank anything a second time.
        let again = task_move(&conn, t.id, "done", 2.0).unwrap();
        assert_eq!(again.spent_ms, total);
        assert_eq!(again.started_at, None);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn timer_state_is_carried_through_dto_json_and_trash_snapshot() {
        let (wd, conn, pid) = setup("tasks-timer-dto");
        let t = task_create(&conn, pid, "t", Some("progress".into())).unwrap();
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["spentMs"], 0);
        assert!(json["startedAt"].is_i64(), "the DTO must expose the running start");

        let (project_id, trash_id) = task_delete_with_trash(&conn, t.id).unwrap();
        assert_eq!(project_id, pid);
        let raw: String = conn
            .query_row("SELECT meta FROM trash WHERE id = ?1", params![trash_id], |r| r.get(0))
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(meta["spentMs"], 0);
        assert_eq!(meta["startedAt"], json["startedAt"], "the trash snapshot must keep the timer");
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
    fn overdue_boundary_compares_date_plus_time() {
        // Pure boundary check around the 07:00 default: at 07:00:00 sharp the
        // task is not yet overdue, one minute later it is.
        let now = "2099-01-02 07:00:00";
        assert!(is_overdue("new", Some("2099-01-02"), "06:59", now));
        assert!(!is_overdue("new", Some("2099-01-02"), "07:00", now));
        assert!(!is_overdue("new", Some("2099-01-02"), "07:01", now));
        assert!(!is_overdue("done", Some("2099-01-02"), "06:59", now));
        assert!(!is_overdue("new", None, "06:59", now));
        assert!(is_overdue("progress", Some("2099-01-01"), "23:59", now));
        assert!(!is_overdue("progress", Some("2099-01-03"), "00:00", now));

        // Against the live DB clock: today at one minute ago is overdue, at
        // one minute ahead is not, whatever time of day the test runs at.
        let (wd, conn, pid) = setup("tasks-overdue-time");
        let (past_date, past_time) = conn
            .query_row(
                "SELECT date('now','localtime','-1 minute'), strftime('%H:%M','now','localtime','-1 minute')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (future_date, future_time) = conn
            .query_row(
                "SELECT date('now','localtime','+1 minute'), strftime('%H:%M','now','localtime','+1 minute')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let late = task_create_with(&conn, pid, "minute-ago", None, Some(past_date), Some(past_time), None, None).unwrap();
        assert!(late.overdue, "due one minute ago must be overdue");
        let ahead = task_create_with(&conn, pid, "minute-ahead", None, Some(future_date), Some(future_time), None, None).unwrap();
        assert!(!ahead.overdue, "due one minute ahead must not be overdue");
        let listed = tasks_list(&conn, pid).unwrap();
        assert!(listed.iter().any(|t| t.id == late.id && t.overdue));
        assert!(listed.iter().any(|t| t.id == ahead.id && !t.overdue));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn due_time_defaults_to_0700_and_persists() {
        let (wd, conn, pid) = setup("tasks-due-time");
        // Absent time on create -> 07:00, even with a due date.
        let t = task_create_with(&conn, pid, "plain", None, Some("2099-01-02".into()), None, None, None).unwrap();
        assert_eq!(t.due_time, "07:00");
        assert_eq!(serde_json::to_value(&t).unwrap()["dueTime"], "07:00");
        let explicit = task_create_with(&conn, pid, "timed", None, Some("2099-01-02".into()), Some("23:45".into()), None, None).unwrap();
        assert_eq!(explicit.due_time, "23:45");

        // Update sets it; absent or explicit null keeps it (NOT NULL column).
        let patched: TaskPatch = serde_json::from_str(r#"{"dueTime": "10:30"}"#).unwrap();
        let patched = task_update(&conn, t.id, patched).unwrap();
        assert_eq!(patched.due_time, "10:30");
        let kept: TaskPatch = serde_json::from_str(r#"{"dueTime": null, "title": "renamed"}"#).unwrap();
        let kept = task_update(&conn, t.id, kept).unwrap();
        assert_eq!(kept.due_time, "10:30");
        let kept = task_update(&conn, t.id, TaskPatch::default()).unwrap();
        assert_eq!(kept.due_time, "10:30");

        // Malformed times are rejected and change nothing.
        for bad in ["7:00", "07:60", "24:00", "0700", "07:0a", "07:00:00", ""] {
            assert!(
                matches!(
                    task_update(&conn, t.id, TaskPatch { due_time: Some(bad.into()), ..Default::default() }),
                    Err(AppError::Invalid(_))
                ),
                "accepted due_time {bad:?}"
            );
            assert!(
                matches!(
                    task_create_with(&conn, pid, "x", None, None, Some(bad.into()), None, None),
                    Err(AppError::Invalid(_))
                ),
                "accepted due_time {bad:?} on create"
            );
        }
        assert_eq!(dto(&conn, t.id).unwrap().due_time, "10:30");
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
                is_priority: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

        let empty: TaskPatch = serde_json::from_str("{}").unwrap();
        let kept = task_update(&conn, t.id, empty).unwrap();
        assert_eq!(kept.comment.as_deref(), Some("note"));
        assert_eq!(kept.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(kept.remind_at.as_deref(), Some("2099-01-02T08:00"));
        assert!(kept.is_priority);

        let cleared: TaskPatch =
            serde_json::from_str(r#"{"comment": null, "dueDate": null, "remindAt": null}"#).unwrap();
        let cleared = task_update(&conn, t.id, cleared).unwrap();
        assert_eq!(cleared.comment, None);
        assert_eq!(cleared.due_date, None);
        assert_eq!(cleared.remind_at, None);
        assert!(cleared.is_priority);

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

    #[test]
    fn is_priority_and_checklist_round_trip() {
        let (wd, conn, pid) = setup("tasks-upgrade-fields");
        let t = task_create_with(
            &conn,
            pid,
            "report",
            None,
            Some("2099-01-02".into()),
            None,
            Some(true),
            Some(vec![
                ChecklistItem { text: " draft ".into(), done: true },
                ChecklistItem { text: "send".into(), done: false },
            ]),
        )
        .unwrap();
        assert!(t.is_priority);
        assert_eq!(t.due_date.as_deref(), Some("2099-01-02"));
        assert_eq!(
            t.checklist,
            vec![
                ChecklistItem { text: "draft".into(), done: true },
                ChecklistItem { text: "send".into(), done: false },
            ]
        );
        assert!(tasks_list(&conn, pid).unwrap()[0].is_priority);

        // camelCase JSON patch: absent fields keep values, set fields replace.
        let patched: TaskPatch = serde_json::from_str(
            r#"{"isPriority": false, "checklist": [{"text": "only", "done": true}]}"#,
        )
        .unwrap();
        let patched = task_update(&conn, t.id, patched).unwrap();
        assert!(!patched.is_priority);
        assert_eq!(patched.checklist, vec![ChecklistItem { text: "only".into(), done: true }]);

        // Defaults on plain create; null for NOT NULL columns keeps values.
        let plain = task_create(&conn, pid, "plain", None).unwrap();
        assert!(!plain.is_priority);
        assert_eq!(plain.due_date, None);
        assert!(plain.checklist.is_empty());
        let kept: TaskPatch =
            serde_json::from_str(r#"{"isPriority": null, "checklist": null}"#).unwrap();
        let kept = task_update(&conn, t.id, kept).unwrap();
        assert!(!kept.is_priority);
        assert_eq!(kept.checklist.len(), 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn checklist_is_validated() {
        let (wd, conn, pid) = setup("tasks-upgrade-invalid");
        let t = task_create(&conn, pid, "t", None).unwrap();
        // A bad due date on create is rejected before any insert.
        assert!(matches!(
            task_create_with(&conn, pid, "t", None, Some("2099-13-01".into()), None, None, None),
            Err(AppError::Invalid(_))
        ));
        let too_many = vec![ChecklistItem { text: "x".into(), done: false }; MAX_CHECKLIST_ITEMS + 1];
        assert!(matches!(
            task_update(&conn, t.id, TaskPatch { checklist: Some(too_many), ..Default::default() }),
            Err(AppError::Invalid(_))
        ));
        // Blank texts are dropped, so the cap counts real items only.
        let blanks = vec![ChecklistItem { text: "  ".into(), done: false }; MAX_CHECKLIST_ITEMS + 1];
        let ok = task_update(&conn, t.id, TaskPatch { checklist: Some(blanks), ..Default::default() }).unwrap();
        assert!(ok.checklist.is_empty());
        let at_cap = vec![ChecklistItem { text: "x".into(), done: false }; MAX_CHECKLIST_ITEMS];
        assert_eq!(
            task_update(&conn, t.id, TaskPatch { checklist: Some(at_cap), ..Default::default() })
                .unwrap()
                .checklist
                .len(),
            MAX_CHECKLIST_ITEMS
        );
        // Failed patches must not change stored values.
        let fresh = dto(&conn, t.id).unwrap();
        assert!(!fresh.is_priority);
        assert_eq!(fresh.checklist.len(), MAX_CHECKLIST_ITEMS);
        fs::remove_dir_all(&wd).unwrap();
    }

}
