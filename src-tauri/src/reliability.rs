// Trash and version history (SPEC trash/versions slice): every delete moves
// the item into Settings/.trash/<id>/<name> instead of removing it, and every
// overwrite of an existing file snapshots the previous content into
// Settings/.versions/<id> before writing. Task deletes keep a JSON snapshot
// in the trash row itself (kind='task', empty stored_path). Both stores live
// under the working dir and are pruned on startup (30 days) and per path
// (20 versions).
use crate::error::{AppError, Result};
use crate::fs_ops;
use crate::paths;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const TRASH_RETENTION_DAYS: u32 = 30;
pub const VERSION_RETENTION_DAYS: u32 = 30;
pub const MAX_VERSIONS_PER_PATH: i64 = 20;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashDto {
    pub id: i64,
    pub scope: String,
    pub kind: String,
    pub original_path: String,
    pub stored_path: String,
    pub name: String,
    pub deleted_at: String,
    pub meta: Option<String>,
    // Filled only when a task snapshot is restored, so callers can emit
    // tasks-changed for the owning project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionDto {
    pub id: i64,
    pub scope: String,
    pub path: String,
    pub bytes: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionRestoreDto {
    pub id: i64,
    pub scope: String,
    pub path: String,
    pub content: String,
}

// Unique store folder/file name. No uuid crate: nanos + a process counter,
// the same pattern the edit/ops temp names use.
fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:04x}", nanos, COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

// rename first; on cross-device sources (harness fs_delete reaches the whole
// machine) fall back to copy + remove so the item still lands in the trash.
fn move_item(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            let copied = if src.is_dir() {
                fs_ops::copy_tree(src, dst)
            } else {
                fs::copy(src, dst).map(|_| ()).map_err(AppError::from)
            };
            match copied {
                Ok(()) => {
                    if src.is_dir() {
                        fs::remove_dir_all(src)?;
                    } else {
                        fs::remove_file(src)?;
                    }
                    Ok(())
                }
                Err(e) => {
                    let _ = fs::remove_dir_all(dst);
                    Err(e)
                }
            }
        }
    }
}

fn map_trash_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrashDto> {
    Ok(TrashDto {
        id: row.get(0)?,
        scope: row.get(1)?,
        kind: row.get(2)?,
        original_path: row.get(3)?,
        stored_path: row.get(4)?,
        name: row.get(5)?,
        deleted_at: row.get(6)?,
        meta: row.get(7)?,
        project_id: None,
    })
}

const TRASH_COLUMNS: &str = "id, scope, kind, original_path, stored_path, name, deleted_at, meta";

fn fetch_trash(conn: &Connection, id: i64) -> Result<TrashDto> {
    conn.query_row(
        &format!("SELECT {TRASH_COLUMNS} FROM trash WHERE id = ?1"),
        params![id],
        map_trash_row,
    )
    .optional()?
    .ok_or_else(|| AppError::Invalid(format!("trash entry {id} not found")))
}

// Moves the existing item at `original_path` into Settings/.trash/<id>/<name>
// and records the row; the caller gets the trash id back.
pub fn trash_put(
    conn: &Connection,
    wd: &Path,
    scope: &str,
    kind: &str,
    original_path: &Path,
    meta: Option<&str>,
) -> Result<i64> {
    let raw_name = original_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = fs_ops::check_name(&raw_name)?;
    let trash = paths::trash_dir(wd);
    fs::create_dir_all(&trash)?;
    let stored_dir = trash.join(unique_id());
    fs::create_dir_all(&stored_dir)?;
    let stored = stored_dir.join(&name);
    move_item(original_path, &stored)?;
    conn.execute(
        "INSERT INTO trash (scope, kind, original_path, stored_path, name, deleted_at, meta)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now','localtime'), ?6)",
        params![scope, kind, original_path.display().to_string(), stored.display().to_string(), name, meta],
    )?;
    Ok(conn.last_insert_rowid())
}

// DB-only snapshot (deleted task): the whole item lives in `meta`, no file is
// moved, stored_path stays empty.
pub fn trash_put_meta(
    conn: &Connection,
    scope: &str,
    kind: &str,
    original_path: &str,
    name: &str,
    meta: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO trash (scope, kind, original_path, stored_path, name, deleted_at, meta)
         VALUES (?1, ?2, ?3, '', ?4, datetime('now','localtime'), ?5)",
        params![scope, kind, original_path, name, meta],
    )?;
    Ok(conn.last_insert_rowid())
}

// Newest deletions first.
pub fn trash_list(conn: &Connection) -> Result<Vec<TrashDto>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRASH_COLUMNS} FROM trash ORDER BY deleted_at DESC, id DESC"
    ))?;
    let rows = stmt.query_map([], map_trash_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// Deletes the stored tree (guarded to the trash folder) and the row.
fn drop_stored(wd: &Path, row: &TrashDto) -> Result<()> {
    if row.stored_path.is_empty() {
        return Ok(());
    }
    let stored = Path::new(&row.stored_path);
    if !stored.starts_with(paths::trash_dir(wd)) {
        return Err(AppError::Invalid(format!(
            "trash entry {} points outside the trash folder",
            row.id
        )));
    }
    if let Some(dir) = stored.parent() {
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

// Puts the item back at its original path (parents are recreated); a task
// snapshot is re-inserted with the same id when the slot is free.
pub fn trash_restore(conn: &Connection, wd: &Path, id: i64) -> Result<TrashDto> {
    let row = fetch_trash(conn, id)?;
    let mut dto = row.clone();
    if row.kind == "task" {
        let project_id = crate::tasks::task_restore_from_trash(conn, &row)?;
        dto.project_id = Some(project_id);
        conn.execute("DELETE FROM trash WHERE id = ?1", params![id])?;
        return Ok(dto);
    }
    let dest = PathBuf::from(&row.original_path);
    if fs::symlink_metadata(&dest).is_ok() {
        return Err(AppError::Invalid(format!("\"{}\" already exists", row.name)));
    }
    if !Path::new(&row.stored_path).starts_with(paths::trash_dir(wd)) {
        return Err(AppError::Invalid(format!(
            "trash entry {id} points outside the trash folder"
        )));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    move_item(Path::new(&row.stored_path), &dest)?;
    conn.execute("DELETE FROM trash WHERE id = ?1", params![id])?;
    let _ = fs::remove_dir_all(
        Path::new(&row.stored_path)
            .parent()
            .unwrap_or(&paths::trash_dir(wd)),
    );
    Ok(dto)
}

pub fn trash_purge(conn: &Connection, wd: &Path, id: i64) -> Result<()> {
    let row = fetch_trash(conn, id)?;
    conn.execute("DELETE FROM trash WHERE id = ?1", params![id])?;
    drop_stored(wd, &row)?;
    Ok(())
}

// Drops entries older than `days`; 0 disables the purge.
pub fn trash_purge_old(conn: &Connection, wd: &Path, days: u32) -> Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let rows: Vec<TrashDto> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {TRASH_COLUMNS} FROM trash WHERE deleted_at < datetime('now','localtime', ?1)"
        ))?;
        let mapped = stmt.query_map(params![format!("-{days} days")], map_trash_row)?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for row in &rows {
        conn.execute("DELETE FROM trash WHERE id = ?1", params![row.id])?;
        drop_stored(wd, row)?;
    }
    Ok(rows.len())
}

// Snapshots `previous_content` (the content about to be overwritten at
// `path`) and keeps only the newest MAX_VERSIONS_PER_PATH rows per path.
pub fn version_save(
    conn: &Connection,
    wd: &Path,
    scope: &str,
    path: &str,
    previous_content: &str,
) -> Result<i64> {
    let dir = paths::versions_dir(wd);
    fs::create_dir_all(&dir)?;
    let stored = dir.join(unique_id());
    fs::write(&stored, previous_content)?;
    conn.execute(
        "INSERT INTO versions (scope, path, stored_path, bytes, created_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now','localtime'))",
        params![scope, path, stored.display().to_string(), previous_content.len() as i64],
    )?;
    let id = conn.last_insert_rowid();
    prune_versions_path(conn, wd, scope, path)?;
    Ok(id)
}

fn drop_version_file(wd: &Path, stored: &str) -> Result<()> {
    let file = Path::new(stored);
    if !file.starts_with(paths::versions_dir(wd)) {
        return Err(AppError::Invalid(
            "version snapshot points outside the versions folder".into(),
        ));
    }
    let _ = fs::remove_file(file);
    Ok(())
}

fn prune_versions_path(conn: &Connection, wd: &Path, scope: &str, path: &str) -> Result<()> {
    let stale: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, stored_path FROM versions
             WHERE scope = ?1 AND path = ?2
             ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?3",
        )?;
        let mapped = stmt.query_map(params![scope, path, MAX_VERSIONS_PER_PATH], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, stored) in &stale {
        conn.execute("DELETE FROM versions WHERE id = ?1", params![id])?;
        drop_version_file(wd, stored)?;
    }
    Ok(())
}

// Newest snapshots first, for one (scope, path) pair.
pub fn version_list(conn: &Connection, scope: &str, path: &str) -> Result<Vec<VersionDto>> {
    let mut stmt = conn.prepare(
        "SELECT id, scope, path, bytes, created_at FROM versions
         WHERE scope = ?1 AND path = ?2 ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt.query_map(params![scope, path], |r| {
        Ok(VersionDto {
            id: r.get(0)?,
            scope: r.get(1)?,
            path: r.get(2)?,
            bytes: r.get(3)?,
            created_at: r.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// Returns the stored content; writing it back to `path` is the caller's job
// (so the normal write path and its own version snapshot apply).
pub fn version_restore(conn: &Connection, wd: &Path, id: i64) -> Result<VersionRestoreDto> {
    let (scope, path, stored): (String, String, String) = conn
        .query_row(
            "SELECT scope, path, stored_path FROM versions WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::Invalid(format!("version {id} not found")))?;
    let file = Path::new(&stored);
    if !file.starts_with(paths::versions_dir(wd)) {
        return Err(AppError::Invalid(format!(
            "version {id} points outside the versions folder"
        )));
    }
    let content = fs::read_to_string(file)
        .map_err(|_| AppError::Invalid(format!("version {id} snapshot is missing on disk")))?;
    Ok(VersionRestoreDto { id, scope, path, content })
}

// Drops snapshots older than `days`; 0 disables the purge.
pub fn version_prune_old(conn: &Connection, wd: &Path, days: u32) -> Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let rows: Vec<(i64, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, stored_path FROM versions WHERE created_at < datetime('now','localtime', ?1)")?;
        let mapped = stmt.query_map(params![format!("-{days} days")], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, stored) in &rows {
        conn.execute("DELETE FROM versions WHERE id = ?1", params![id])?;
        drop_version_file(wd, stored)?;
    }
    Ok(rows.len())
}

// Startup hygiene next to the tmp/journal purges: best-effort, never blocks.
pub fn startup_purge(conn: &Connection, wd: &Path) {
    let _ = trash_purge_old(conn, wd, TRASH_RETENTION_DAYS);
    let _ = version_prune_old(conn, wd, VERSION_RETENTION_DAYS);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::onboarding::temp_test_dir;
    use crate::projects;
    use crate::tasks;

    fn setup(tag: &str) -> (PathBuf, Connection) {
        let wd = temp_test_dir(tag);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        (wd, conn)
    }

    #[test]
    fn trash_put_moves_file_and_restore_puts_it_back() {
        let (wd, conn) = setup("rel-trash-roundtrip");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let file = root.join("a.txt");
        fs::write(&file, "содержимое").unwrap();

        let id = trash_put(&conn, &wd, "project", "file", &file, None).unwrap();
        assert!(!file.exists());
        let list = trash_list(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].name, "a.txt");
        assert!(Path::new(&list[0].stored_path).is_file());

        let dto = trash_restore(&conn, &wd, id).unwrap();
        assert_eq!(dto.name, "a.txt");
        assert_eq!(fs::read_to_string(&file).unwrap(), "содержимое");
        assert!(trash_list(&conn).unwrap().is_empty());
        // The uuid folder is gone once the item moved back.
        assert_eq!(fs::read_dir(paths::trash_dir(&wd)).unwrap().count(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trash_restore_recreates_missing_parents_and_refuses_existing_dest() {
        let (wd, conn) = setup("rel-trash-parents");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let dir = root.join("docs");
        let file = dir.join("a.txt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, "x").unwrap();

        let id = trash_put(&conn, &wd, "project", "dir", &dir, None).unwrap();
        assert!(!dir.exists());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "конфликт").unwrap();
        let err = trash_restore(&conn, &wd, id).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        fs::remove_dir_all(&dir).unwrap();
        trash_restore(&conn, &wd, id).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "x");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trash_purge_removes_row_and_stored_tree() {
        let (wd, conn) = setup("rel-trash-purge");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let file = root.join("a.txt");
        fs::write(&file, "x").unwrap();
        let id = trash_put(&conn, &wd, "project", "file", &file, None).unwrap();
        let stored = trash_list(&conn).unwrap()[0].stored_path.clone();

        trash_purge(&conn, &wd, id).unwrap();
        assert!(trash_list(&conn).unwrap().is_empty());
        assert!(!Path::new(&stored).exists());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trash_purge_old_only_drops_backdated_entries() {
        let (wd, conn) = setup("rel-trash-old");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let fresh = root.join("fresh.txt");
        let old = root.join("old.txt");
        fs::write(&fresh, "f").unwrap();
        fs::write(&old, "o").unwrap();
        trash_put(&conn, &wd, "project", "file", &fresh, None).unwrap();
        let old_id = trash_put(&conn, &wd, "project", "file", &old, None).unwrap();
        conn.execute(
            "UPDATE trash SET deleted_at = datetime('now','localtime','-40 days') WHERE id = ?1",
            params![old_id],
        )
        .unwrap();

        assert_eq!(trash_purge_old(&conn, &wd, 30).unwrap(), 1);
        let rest = trash_list(&conn).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].name, "fresh.txt");
        assert_eq!(trash_purge_old(&conn, &wd, 0).unwrap(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn harness_scope_delete_and_restore_roundtrip() {
        // The harness fs_delete tool reaches the whole machine; the trash
        // store still lives under the working dir.
        let (wd, conn) = setup("rel-harness-trash");
        let harness = paths::harness_dir(&wd);
        fs::create_dir_all(&harness).unwrap();
        let file = harness.join("report.md");
        fs::write(&file, "отчёт").unwrap();

        let id = trash_put(&conn, &wd, "harness", "file", &file, None).unwrap();
        assert!(!file.exists());
        let row = trash_list(&conn).unwrap().remove(0);
        assert_eq!(row.scope, "harness");
        assert_eq!(row.id, id);
        trash_restore(&conn, &wd, id).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "отчёт");
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_delete_snapshots_to_trash_and_restore_keeps_the_same_id() {
        let (wd, conn) = setup("rel-task-trash");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let task = tasks::task_create(&conn, project.id, "сделать отчёт", None).unwrap();

        let project_id = tasks::task_delete(&conn, task.id).unwrap();
        assert_eq!(project_id, project.id);
        let list = trash_list(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, "task");
        assert!(list[0].stored_path.is_empty());
        let meta: serde_json::Value = serde_json::from_str(list[0].meta.as_deref().unwrap()).unwrap();
        assert_eq!(meta["title"], "сделать отчёт");

        let dto = trash_restore(&conn, &wd, list[0].id).unwrap();
        assert_eq!(dto.project_id, Some(project.id));
        let tasks = tasks::tasks_list(&conn, project.id).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id, "restore must reuse the original id");
        assert_eq!(tasks[0].title, "сделать отчёт");
        assert!(trash_list(&conn).unwrap().is_empty());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn task_restore_after_id_taken_inserts_with_a_new_id() {
        let (wd, conn) = setup("rel-task-id-taken");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let task = tasks::task_create(&conn, project.id, "старая", None).unwrap();
        tasks::task_delete(&conn, task.id).unwrap();
        let blocker = tasks::task_create(&conn, project.id, "новая", None).unwrap();
        assert_eq!(blocker.id, task.id, "sqlite reuses the freed id");

        let trash_id = trash_list(&conn).unwrap()[0].id;
        trash_restore(&conn, &wd, trash_id).unwrap();
        let tasks = tasks::tasks_list(&conn, project.id).unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t.id != blocker.id && t.title == "старая"));
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn version_save_snapshots_previous_content_and_restore_returns_it() {
        let (wd, conn) = setup("rel-version-roundtrip");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let file = root.join("a.txt");
        let path = file.display().to_string();

        fs::write(&file, "v1").unwrap();
        version_save(&conn, &wd, "project", &path, "v1").unwrap();
        fs::write(&file, "v2").unwrap();
        version_save(&conn, &wd, "project", &path, "v2").unwrap();

        let list = version_list(&conn, "project", &path).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].bytes, 2);
        let restored = version_restore(&conn, &wd, list[1].id).unwrap();
        assert_eq!(restored.content, "v1");
        assert_eq!(restored.path, path);
        // The snapshot row survives a restore (non-destructive).
        assert_eq!(version_list(&conn, "project", &path).unwrap().len(), 2);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn version_save_prunes_to_the_newest_twenty_per_path() {
        let (wd, conn) = setup("rel-version-cap");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let file = root.join("a.txt");
        let path = file.display().to_string();
        let mut stored = Vec::new();
        for i in 0..25 {
            let id = version_save(&conn, &wd, "project", &path, &format!("v{i}")).unwrap();
            let sp: String = conn
                .query_row("SELECT stored_path FROM versions WHERE id = ?1", params![id], |r| r.get(0))
                .unwrap();
            stored.push((id, sp));
        }
        let list = version_list(&conn, "project", &path).unwrap();
        assert_eq!(list.len(), MAX_VERSIONS_PER_PATH as usize);
        assert_eq!(list[0].bytes, 3); // "v24"
        // The five oldest snapshot files are gone from disk.
        for (id, sp) in &stored[..5] {
            assert!(!Path::new(sp).exists(), "stale snapshot {id} survived the cap");
        }
        for (_, sp) in &stored[5..] {
            assert!(Path::new(sp).exists());
        }
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn version_prune_old_only_drops_backdated_snapshots() {
        let (wd, conn) = setup("rel-version-old");
        let project = projects::create_project(&conn, &wd, "example-ru").unwrap();
        let root = projects::project_root(&conn, &wd, project.id).unwrap();
        let path = root.join("a.txt").display().to_string();
        version_save(&conn, &wd, "project", &path, "fresh").unwrap();
        let old_id = version_save(&conn, &wd, "project", &path, "old").unwrap();
        conn.execute(
            "UPDATE versions SET created_at = datetime('now','localtime','-40 days') WHERE id = ?1",
            params![old_id],
        )
        .unwrap();

        assert_eq!(version_prune_old(&conn, &wd, 30).unwrap(), 1);
        let rest = version_list(&conn, "project", &path).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].bytes, 5);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn trash_restore_rejects_entries_outside_the_trash_folder() {
        let (wd, conn) = setup("rel-trash-escape");
        conn.execute(
            "INSERT INTO trash (scope, kind, original_path, stored_path, name, deleted_at)
             VALUES ('harness', 'file', '/tmp/does-not-exist-s', '/etc/passwd', 'passwd', datetime('now'))",
            [],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let err = trash_restore(&conn, &wd, id).unwrap_err();
        assert!(err.to_string().contains("outside the trash folder"), "{err}");
        fs::remove_dir_all(&wd).unwrap();
    }
}
