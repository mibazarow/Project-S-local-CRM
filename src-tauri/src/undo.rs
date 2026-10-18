use crate::error::{AppError, Result};
use crate::reliability;
use crate::tasks;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub const UNDO_LIMIT: i64 = 50;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoEntryDto {
    pub id: i64,
    pub kind: String,
    pub description: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoOutcome {
    pub undone_kind: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_event: Option<UndoFsEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes_rel_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_id: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoFsEvent {
    pub project_id: i64,
    pub rel_path: String,
}

pub fn push(conn: &Connection, kind: &str, payload: Value) -> Result<()> {
    conn.execute(
        "INSERT INTO undo_log (kind, payload, created_at)
         VALUES (?1, ?2, datetime('now','localtime'))",
        params![kind, payload.to_string()],
    )?;
    conn.execute(
        "DELETE FROM undo_log
         WHERE id NOT IN (
             SELECT id FROM undo_log
             ORDER BY created_at DESC, id DESC
             LIMIT ?1
         )",
        params![UNDO_LIMIT],
    )?;
    Ok(())
}

pub fn push_file_write(
    conn: &Connection,
    scope: &str,
    path: &str,
    version_id: Option<i64>,
    created: bool,
    project_id: Option<i64>,
    rel_path: Option<&str>,
) -> Result<()> {
    push(
        conn,
        "file_write",
        json!({
            "scope": scope,
            "path": path,
            "versionId": version_id,
            "created": created,
            "projectId": project_id,
            "relPath": rel_path,
        }),
    )
}

pub fn push_delete(
    conn: &Connection,
    trash_id: i64,
    scope: &str,
    path: &str,
    project_id: Option<i64>,
    rel_path: Option<&str>,
) -> Result<()> {
    push(
        conn,
        "delete",
        json!({
            "trashId": trash_id,
            "scope": scope,
            "path": path,
            "projectId": project_id,
            "relPath": rel_path,
        }),
    )
}

pub fn push_rename(
    conn: &Connection,
    scope: &str,
    from: &str,
    to: &str,
    project_id: Option<i64>,
    rel_path: Option<&str>,
) -> Result<()> {
    push(
        conn,
        "rename",
        json!({
            "scope": scope,
            "from": from,
            "to": to,
            "projectId": project_id,
            "relPath": rel_path,
        }),
    )
}

pub fn push_note_move(conn: &Connection, from: &str, to: &str, rel_path: &str) -> Result<()> {
    push(
        conn,
        "note_move",
        json!({ "from": from, "to": to, "relPath": rel_path }),
    )
}

pub fn push_note_copy(conn: &Connection, created_path: &str, rel_path: &str) -> Result<()> {
    push(
        conn,
        "note_copy",
        json!({ "createdPath": created_path, "relPath": rel_path }),
    )
}

pub fn push_task_create(conn: &Connection, task_id: i64, project_id: i64) -> Result<()> {
    push(
        conn,
        "task_create",
        json!({ "taskId": task_id, "projectId": project_id }),
    )
}

pub fn push_task_snapshot(
    conn: &Connection,
    kind: &str,
    snapshot: &tasks::TaskSnapshot,
    spawned_task_id: Option<i64>,
) -> Result<()> {
    push(
        conn,
        kind,
        json!({
            "snapshot": serde_json::to_value(snapshot)?,
            "spawnedTaskId": spawned_task_id,
        }),
    )
}

pub fn push_task_delete(conn: &Connection, trash_id: i64, project_id: i64) -> Result<()> {
    push(
        conn,
        "task_delete",
        json!({ "trashId": trash_id, "projectId": project_id }),
    )
}

pub fn list(conn: &Connection, limit: Option<usize>) -> Result<Vec<UndoEntryDto>> {
    let limit = limit
        .unwrap_or(20)
        .clamp(1, UNDO_LIMIT as usize)
        .min(i64::MAX as usize) as i64;
    let mut stmt = conn.prepare(
        "SELECT id, kind, payload, created_at
         FROM undo_log
         ORDER BY created_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        let id: i64 = row.get(0)?;
        let kind: String = row.get(1)?;
        let payload: String = row.get(2)?;
        let created_at: String = row.get(3)?;
        Ok((id, kind, payload, created_at))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, payload, created_at) = row?;
        out.push(UndoEntryDto {
            id,
            kind: kind.clone(),
            description: description_for(&kind, &payload),
            created_at,
        });
    }
    Ok(out)
}

pub fn undo_last(conn: &Connection, wd: &Path) -> Result<UndoOutcome> {
    let (id, kind, payload) = conn
        .query_row(
            "SELECT id, kind, payload FROM undo_log ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid("nothing to undo".into()))?;
    let value: Value = serde_json::from_str(&payload)
        .map_err(|e| AppError::Invalid(format!("bad undo payload: {e}")))?;
    let outcome = undo_value(conn, wd, &kind, &value)?;
    conn.execute("DELETE FROM undo_log WHERE id = ?1", params![id])?;
    Ok(outcome)
}

fn undo_value(conn: &Connection, wd: &Path, kind: &str, payload: &Value) -> Result<UndoOutcome> {
    match kind {
        "file_write" => undo_file_write(conn, wd, payload),
        "delete" => undo_delete(conn, wd, payload),
        "rename" => undo_rename(payload),
        "note_move" => undo_note_move(payload),
        "note_copy" => undo_note_copy(conn, wd, payload),
        "task_create" => undo_task_create(conn, payload),
        "task_update" | "task_move" => undo_task_snapshot(conn, payload, kind),
        "task_delete" => undo_task_delete(conn, wd, payload),
        other => Err(AppError::Invalid(format!("unknown undo kind \"{other}\""))),
    }
}

fn undo_file_write(conn: &Connection, wd: &Path, payload: &Value) -> Result<UndoOutcome> {
    let scope = payload["scope"].as_str().unwrap_or("harness").to_owned();
    let path = payload["path"]
        .as_str()
        .ok_or_else(|| AppError::Invalid("undo file_write: missing path".into()))?
        .to_owned();
    let version_id = payload["versionId"].as_i64();
    let created = payload["created"].as_bool().unwrap_or(false);
    let project_id = payload["projectId"].as_i64();
    let rel_path = payload["relPath"].as_str().map(str::to_owned);
    if let Some(version_id) = version_id {
        let restored = reliability::version_restore(conn, wd, version_id)?;
        let target = Path::new(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, restored.content)?;
    } else if created {
        let target = Path::new(&path);
        if fs::symlink_metadata(target).is_ok() {
            let trash_id = reliability::trash_put(conn, wd, &scope, "file", target, None)?;
            return Ok(UndoOutcome {
                undone_kind: "file_write".into(),
                description: description_for("file_write", &payload.to_string()),
                fs_event: fs_event(project_id, rel_path.as_deref()),
                notes_rel_path: (scope == "notes").then(|| rel_path.clone()).flatten(),
                task_project_id: None,
                trash_id: Some(trash_id),
            });
        }
    } else {
        return Err(AppError::Invalid(
            "undo file_write: no version and not created".into(),
        ));
    }
    Ok(UndoOutcome {
        undone_kind: "file_write".into(),
        description: description_for("file_write", &payload.to_string()),
        fs_event: fs_event(project_id, rel_path.as_deref()),
        notes_rel_path: (scope == "notes").then(|| rel_path.clone()).flatten(),
        task_project_id: None,
        trash_id: None,
    })
}

fn undo_delete(conn: &Connection, wd: &Path, payload: &Value) -> Result<UndoOutcome> {
    let trash_id = payload["trashId"]
        .as_i64()
        .ok_or_else(|| AppError::Invalid("undo delete: missing trashId".into()))?;
    let scope = payload["scope"].as_str().unwrap_or("harness").to_owned();
    let project_id = payload["projectId"].as_i64();
    let rel_path = payload["relPath"].as_str().map(str::to_owned);
    let restored = reliability::trash_restore(conn, wd, trash_id)?;
    Ok(UndoOutcome {
        undone_kind: "delete".into(),
        description: description_for("delete", &payload.to_string()),
        fs_event: if scope == "project" {
            fs_event(project_id, rel_path.as_deref())
        } else {
            None
        },
        notes_rel_path: (scope == "notes").then(|| rel_path.clone()).flatten(),
        task_project_id: restored.project_id,
        trash_id: Some(trash_id),
    })
}

fn undo_rename(payload: &Value) -> Result<UndoOutcome> {
    let from = payload["from"]
        .as_str()
        .ok_or_else(|| AppError::Invalid("undo rename: missing from".into()))?;
    let to = payload["to"]
        .as_str()
        .ok_or_else(|| AppError::Invalid("undo rename: missing to".into()))?;
    let scope = payload["scope"].as_str().unwrap_or("harness");
    let project_id = payload["projectId"].as_i64();
    let rel_path = payload["relPath"].as_str().map(str::to_owned);
    let target = Path::new(from);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(to, target)?;
    Ok(UndoOutcome {
        undone_kind: "rename".into(),
        description: description_for("rename", &payload.to_string()),
        fs_event: if scope == "project" {
            fs_event(project_id, rel_path.as_deref())
        } else {
            None
        },
        notes_rel_path: (scope == "notes").then(|| rel_path.clone()).flatten(),
        task_project_id: None,
        trash_id: None,
    })
}

fn undo_note_move(payload: &Value) -> Result<UndoOutcome> {
    let from = payload["from"]
        .as_str()
        .ok_or_else(|| AppError::Invalid("undo note_move: missing from".into()))?;
    let to = payload["to"]
        .as_str()
        .ok_or_else(|| AppError::Invalid("undo note_move: missing to".into()))?;
    let rel_path = payload["relPath"].as_str().map(str::to_owned);
    let target = Path::new(from);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(to, target)?;
    Ok(UndoOutcome {
        undone_kind: "note_move".into(),
        description: description_for("note_move", &payload.to_string()),
        fs_event: None,
        notes_rel_path: rel_path,
        task_project_id: None,
        trash_id: None,
    })
}

fn undo_note_copy(conn: &Connection, wd: &Path, payload: &Value) -> Result<UndoOutcome> {
    let created_path = payload["createdPath"]
        .as_str()
        .ok_or_else(|| AppError::Invalid("undo note_copy: missing createdPath".into()))?;
    let rel_path = payload["relPath"].as_str().map(str::to_owned);
    let path = Path::new(created_path);
    let kind = if path.is_dir() { "dir" } else { "file" };
    let trash_id = reliability::trash_put(conn, wd, "notes", kind, path, None)?;
    Ok(UndoOutcome {
        undone_kind: "note_copy".into(),
        description: description_for("note_copy", &payload.to_string()),
        fs_event: None,
        notes_rel_path: rel_path,
        task_project_id: None,
        trash_id: Some(trash_id),
    })
}

fn undo_task_create(conn: &Connection, payload: &Value) -> Result<UndoOutcome> {
    let task_id = payload["taskId"]
        .as_i64()
        .ok_or_else(|| AppError::Invalid("undo task_create: missing taskId".into()))?;
    let project_id = payload["projectId"]
        .as_i64()
        .ok_or_else(|| AppError::Invalid("undo task_create: missing projectId".into()))?;
    tasks::task_delete_row(conn, task_id)?;
    Ok(UndoOutcome {
        undone_kind: "task_create".into(),
        description: description_for("task_create", &payload.to_string()),
        fs_event: None,
        notes_rel_path: None,
        task_project_id: Some(project_id),
        trash_id: None,
    })
}

fn undo_task_snapshot(conn: &Connection, payload: &Value, kind: &str) -> Result<UndoOutcome> {
    let snapshot: tasks::TaskSnapshot = serde_json::from_value(payload["snapshot"].clone())
        .map_err(|e| AppError::Invalid(format!("undo task snapshot: {e}")))?;
    tasks::task_restore_snapshot(conn, &snapshot)?;
    if let Some(spawned) = payload["spawnedTaskId"].as_i64() {
        let _ = tasks::task_delete_row(conn, spawned);
    }
    Ok(UndoOutcome {
        undone_kind: kind.to_owned(),
        description: description_for(kind, &payload.to_string()),
        fs_event: None,
        notes_rel_path: None,
        task_project_id: Some(snapshot.project_id),
        trash_id: None,
    })
}

fn undo_task_delete(conn: &Connection, wd: &Path, payload: &Value) -> Result<UndoOutcome> {
    let trash_id = payload["trashId"]
        .as_i64()
        .ok_or_else(|| AppError::Invalid("undo task_delete: missing trashId".into()))?;
    let restored = reliability::trash_restore(conn, wd, trash_id)?;
    Ok(UndoOutcome {
        undone_kind: "task_delete".into(),
        description: description_for("task_delete", &payload.to_string()),
        fs_event: None,
        notes_rel_path: None,
        task_project_id: restored.project_id,
        trash_id: Some(trash_id),
    })
}

fn fs_event(project_id: Option<i64>, rel_path: Option<&str>) -> Option<UndoFsEvent> {
    Some(UndoFsEvent {
        project_id: project_id?,
        rel_path: rel_path?.to_owned(),
    })
}

fn description_for(kind: &str, payload: &str) -> String {
    let value: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let short = |key: &str| value[key].as_str().unwrap_or("").to_owned();
    match kind {
        "file_write" => format!("file write: {}", short("path")),
        "delete" => format!("delete: {}", short("path")),
        "rename" => format!("rename: {} -> {}", short("from"), short("to")),
        "note_move" => format!("note move: {} -> {}", short("from"), short("to")),
        "note_copy" => format!("note copy: {}", short("createdPath")),
        "task_create" => format!("task create: #{}", value["taskId"].as_i64().unwrap_or(0)),
        "task_update" | "task_move" | "task_snapshot" => {
            format!("task restore: #{}", value["snapshot"]["id"].as_i64().unwrap_or(0))
        }
        "task_delete" => format!("task delete: trash #{}", value["trashId"].as_i64().unwrap_or(0)),
        _ => kind.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use crate::projects;
    use std::fs;
    use std::path::PathBuf;

    fn setup(tag: &str) -> (PathBuf, rusqlite::Connection) {
        let wd = temp_test_dir(tag);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        (wd, conn)
    }

    #[test]
    fn file_write_undo_restores_previous_content() {
        let (wd, conn) = setup("undo-file-write");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let file = root.join("a.txt");
        fs::write(&file, "old").unwrap();
        let version_id =
            reliability::version_save(&conn, &wd, "project", &file.display().to_string(), "old")
                .unwrap();
        fs::write(&file, "new").unwrap();
        push_file_write(
            &conn,
            "project",
            &file.display().to_string(),
            Some(version_id),
            false,
            Some(project.id),
            Some("a.txt"),
        )
        .unwrap();
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "file_write");
        assert_eq!(fs::read_to_string(&file).unwrap(), "old");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn delete_undo_restores_trashed_file() {
        let (wd, conn) = setup("undo-delete");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let file = root.join("b.txt");
        fs::write(&file, "keep").unwrap();
        let trash_id =
            reliability::trash_put(&conn, &wd, "project", "file", &file, None).unwrap();
        push_delete(
            &conn,
            trash_id,
            "project",
            &file.display().to_string(),
            Some(project.id),
            Some("b.txt"),
        )
        .unwrap();
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "delete");
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn rename_undo_moves_file_back() {
        let (wd, conn) = setup("undo-rename");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let from = root.join("old.txt");
        let to = root.join("new.txt");
        fs::write(&to, "moved").unwrap();
        push_rename(
            &conn,
            "project",
            &from.display().to_string(),
            &to.display().to_string(),
            Some(project.id),
            Some("old.txt"),
        )
        .unwrap();
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "rename");
        assert_eq!(fs::read_to_string(&from).unwrap(), "moved");
        assert!(!to.exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn notes_move_and_copy_undo() {
        let (wd, conn) = setup("undo-notes");
        let notes = paths::notes_dir(&wd);
        fs::create_dir_all(&notes).unwrap();
        let from = notes.join("a.md");
        let to = notes.join("b.md");
        fs::write(&from, "note").unwrap();
        fs::rename(&from, &to).unwrap();
        push_note_move(
            &conn,
            &from.display().to_string(),
            &to.display().to_string(),
            "a.md",
        )
        .unwrap();
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "note_move");
        assert_eq!(fs::read_to_string(&from).unwrap(), "note");

        let copy = notes.join("copy.md");
        fs::write(&copy, "copy").unwrap();
        push_note_copy(&conn, &copy.display().to_string(), "copy.md").unwrap();
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "note_copy");
        assert!(!copy.exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_create_undo_removes_row() {
        let (wd, conn) = setup("undo-task-create");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let task = tasks::task_create(&conn, project.id, "undo me", None).unwrap();
        push_task_create(&conn, task.id, project.id).unwrap();
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "task_create");
        assert!(tasks::tasks_list(&conn, project.id).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_update_and_move_undo_restore_snapshots() {
        let (wd, conn) = setup("undo-task-snapshot");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let task = tasks::task_create(&conn, project.id, "before", None).unwrap();
        let snapshot = tasks::task_snapshot(&conn, task.id).unwrap();
        tasks::task_update(
            &conn,
            task.id,
            tasks::TaskPatch {
                title: Some("after".into()),
                ..Default::default()
            },
        )
        .unwrap();
        push_task_snapshot(&conn, "task_update", &snapshot, None).unwrap();
        undo_last(&conn, &wd).unwrap();
        assert_eq!(tasks::tasks_list(&conn, project.id).unwrap()[0].title, "before");

        let snapshot = tasks::task_snapshot(&conn, task.id).unwrap();
        tasks::task_move(&conn, task.id, "progress", 1.0).unwrap();
        push_task_snapshot(&conn, "task_move", &snapshot, None).unwrap();
        undo_last(&conn, &wd).unwrap();
        assert_eq!(tasks::tasks_list(&conn, project.id).unwrap()[0].status, "new");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_move_undo_restores_the_wall_clock_timer_state() {
        let (wd, conn) = setup("undo-task-timer");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let task = tasks::task_create(&conn, project.id, "t", None).unwrap();
        // Bank an hour of wall-clock time first (progress -> new).
        tasks::task_move(&conn, task.id, "progress", 1.0).unwrap();
        conn.execute(
            "UPDATE tasks SET started_at = started_at - 3600000 WHERE id = ?1",
            params![task.id],
        )
        .unwrap();
        let stopped = tasks::task_move(&conn, task.id, "new", 1.0).unwrap();
        assert!(stopped.spent_ms >= 3_600_000);

        let snapshot = tasks::task_snapshot(&conn, task.id).unwrap();
        let running = tasks::task_move(&conn, task.id, "progress", 1.0).unwrap();
        assert!(running.started_at.is_some());
        push_task_snapshot(&conn, "task_move", &snapshot, None).unwrap();
        undo_last(&conn, &wd).unwrap();

        let back = tasks::tasks_list(&conn, project.id).unwrap().remove(0);
        assert_eq!(back.status, "new");
        assert_eq!(back.started_at, None, "undo must clear the start stamp the move set");
        assert_eq!(back.spent_ms, snapshot.spent_ms, "undo must restore the banked time exactly");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_delete_undo_restores_the_task_and_clears_the_trash_entry() {
        let (wd, conn) = setup("undo-task-delete");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let task = tasks::task_create(&conn, project.id, "удалить меня", None).unwrap();
        let (pid, trash_id) = tasks::task_delete_with_trash(&conn, task.id).unwrap();
        push_task_delete(&conn, trash_id, pid).unwrap();
        assert!(tasks::tasks_list(&conn, project.id).unwrap().is_empty());
        assert_eq!(reliability::trash_list(&conn).unwrap().len(), 1);
        let out = undo_last(&conn, &wd).unwrap();
        assert_eq!(out.undone_kind, "task_delete");
        assert_eq!(out.task_project_id, Some(project.id));
        assert_eq!(out.trash_id, Some(trash_id));
        let rest = tasks::tasks_list(&conn, project.id).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, task.id, "undo must reuse the original id");
        assert_eq!(rest[0].title, "удалить меня");
        assert!(reliability::trash_list(&conn).unwrap().is_empty());
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM tasks_fts WHERE tasks_fts MATCH ?1",
                params!["удалить"],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "undo must reindex the fts mirror"
        );
        assert!(list(&conn, None).unwrap().is_empty(), "the undo entry must be popped");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn undo_list_orders_newest_first_and_prunes() {
        let (wd, conn) = setup("undo-list");
        for i in 0..(UNDO_LIMIT + 1) {
            push(&conn, "task_create", json!({ "taskId": i, "projectId": 1 })).unwrap();
        }
        let entries = list(&conn, Some(UNDO_LIMIT as usize)).unwrap();
        assert_eq!(entries.len(), UNDO_LIMIT as usize);
        assert_eq!(entries[0].kind, "task_create");
        assert!(list(&conn, Some(1)).unwrap().len() == 1);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn undo_last_reports_nothing_to_undo() {
        let (wd, conn) = setup("undo-empty");
        let err = undo_last(&conn, &wd).unwrap_err();
        assert!(err.to_string().contains("nothing to undo"));
        fs::remove_dir_all(&wd).unwrap();
    }
}
