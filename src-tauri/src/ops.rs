// Operational backend (SPEC 3.6 backup, 5.2 tmp hygiene, 5.3 autostart):
// the pieces the settings UI calls in the next slice.
use crate::config::{self, AppConfig, ConfigKind};
use crate::error::{AppError, Result};
use crate::paths;
use chrono::{DateTime, Local, Utc};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{copy, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use zip::write::FileOptions;
use zip::ZipWriter;

fn zip_err(e: zip::result::ZipError) -> AppError {
    AppError::Io(format!("zip: {e}"))
}

fn unique_temp_db(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("project-s-{label}-{nanos}.db"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub path: String,
    pub entries: usize,
    pub bytes: u64,
}

// SPEC 3.6: one .zip holding a consistent SQLite snapshot (VACUUM INTO, so a
// live WAL DB is copied without locking it) plus copies of Projects/ and the
// Notes/ vault (Harness/tmp is deliberately excluded). Default destination is
// Settings/backups/project-s-YYYYMMDD-HHMMSS.zip. The archive is re-opened
// and the DB inside integrity-checked before success is reported (SPEC 7
// "backup archive is recoverable").
pub fn create_backup(wd: &Path, conn: &Connection, dest: Option<&Path>) -> Result<BackupInfo> {
    let dest = match dest {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = paths::backups_dir(wd);
            fs::create_dir_all(&dir)?;
            dir.join(format!(
                "project-s-{}.zip",
                Local::now().format("%Y%m%d-%H%M%S")
            ))
        }
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let snapshot = unique_temp_db("backup");
    let literal = format!("'{}'", snapshot.display().to_string().replace('\'', "''"));
    conn.execute(&format!("VACUUM INTO {literal}"), [])
        .map_err(|e| AppError::Db(format!("backup snapshot: {e}")))?;

    let result = write_and_verify_zip(wd, &snapshot, &dest);
    let _ = fs::remove_file(&snapshot);
    result
}

fn write_and_verify_zip(wd: &Path, snapshot: &Path, dest: &Path) -> Result<BackupInfo> {
    let opts = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
    let file = File::create(dest)?;
    let mut zip = ZipWriter::new(BufWriter::new(file));

    zip.start_file("project-s.db", opts).map_err(zip_err)?;
    let mut src = File::open(snapshot)?;
    copy(&mut src, &mut zip)?;

    let mut stack = vec![paths::projects_dir(wd), paths::notes_dir(wd)];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(wd)
                    .map_err(|e| AppError::Io(format!("project rel path: {e}")))?
                    .to_string_lossy()
                    .replace('\\', "/");
                zip.start_file(rel, opts).map_err(zip_err)?;
                let mut src = File::open(&path)?;
                copy(&mut src, &mut zip)?;
            }
        }
    }
    zip.finish()
        .map_err(zip_err)?
        .into_inner()
        .map_err(|e| AppError::Io(format!("zip flush: {e}")))?
        .flush()?;

    // Verify: the archive re-opens, carries the DB entry, and the DB inside
    // passes SQLite's integrity check.
    let mut archive = zip::ZipArchive::new(File::open(dest)?).map_err(zip_err)?;
    let entries = archive.len();
    let mut db_bytes = Vec::new();
    archive
        .by_name("project-s.db")
        .map_err(|_| AppError::Invalid("backup archive is missing the DB entry".into()))?
        .read_to_end(&mut db_bytes)?;
    if db_bytes.is_empty() {
        return Err(AppError::Invalid("backup DB entry is empty".into()));
    }
    let check = unique_temp_db("backup-check");
    fs::write(&check, &db_bytes)?;
    crate::db::ensure_vec_extension();
    let check_conn = Connection::open(&check)?;
    let integrity: String = check_conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    drop(check_conn);
    let _ = fs::remove_file(&check);
    if integrity != "ok" {
        return Err(AppError::Invalid(format!(
            "backup DB failed integrity_check: {integrity}"
        )));
    }
    Ok(BackupInfo {
        path: dest.display().to_string(),
        entries,
        bytes: fs::metadata(dest)?.len(),
    })
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TmpStats {
    pub removed_files: u64,
    pub freed_bytes: u64,
}

// SPEC 5.2: wipe the CONTENTS of Harness/tmp/, keep the folder itself.
pub fn clear_tmp(wd: &Path) -> Result<TmpStats> {
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let (removed_files, freed_bytes) = tree_stats(&tmp)?;
    for entry in fs::read_dir(&tmp)? {
        let path = entry?.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(TmpStats {
        removed_files,
        freed_bytes,
    })
}

fn tree_stats(dir: &Path) -> Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                bytes += path.metadata()?.len();
                files += 1;
            }
        }
    }
    Ok((files, bytes))
}

// Startup auto-purge (SPEC 5.2 "автостирание артефактов старше N дней"):
// drop tmp files whose mtime is older than `max_age_days` (0 disables),
// then remove subfolders left empty. The tmp root itself is always kept.
pub fn cleanup_old_tmp(wd: &Path, max_age_days: u32) -> Result<TmpStats> {
    if max_age_days == 0 {
        return Ok(TmpStats::default());
    }
    let tmp = paths::harness_tmp_dir(wd);
    if !tmp.is_dir() {
        return Ok(TmpStats::default());
    }
    let cutoff = SystemTime::now() - Duration::from_secs(u64::from(max_age_days) * 86_400);
    let mut stats = TmpStats::default();
    let mut stack = vec![tmp.clone()];
    let mut dirs = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path.clone());
                stack.push(path);
            } else {
                let meta = path.metadata()?;
                if meta.modified().map(|mtime| mtime < cutoff).unwrap_or(false) {
                    stats.freed_bytes += meta.len();
                    stats.removed_files += 1;
                    fs::remove_file(&path)?;
                }
            }
        }
    }
    // Deepest first so nested empty folders collapse; the tmp root is kept.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        let _ = fs::remove_dir(&dir);
    }
    Ok(stats)
}

// SPEC 5.3: "start with the system" is the OS LaunchAgent; app.json only
// mirrors the user's choice for the UI. Default off (SPEC 8).
pub fn autostart_choice(wd: &Path) -> bool {
    read_app_config(wd).map(|c| c.autostart).unwrap_or(false)
}

pub fn set_autostart_choice(wd: &Path, enabled: bool) -> Result<()> {
    config::write_config(wd, ConfigKind::App, serde_json::json!({ "autostart": enabled }))
}

// Thin seam over the plugin call so the error guard is testable without an
// AppHandle: a missing or failing autostart backend surfaces as an error,
// never a panic.
pub fn apply_autostart(
    driver: &dyn Fn(bool) -> std::result::Result<(), String>,
    enabled: bool,
) -> Result<()> {
    driver(enabled).map_err(|e| AppError::Io(format!("autostart: {e}")))
}

pub fn read_app_config(wd: &Path) -> Result<AppConfig> {
    Ok(serde_json::from_value(config::read_config(wd, ConfigKind::App)?)?)
}

// Automatic backups (SPEC 3.6): on startup and after the working dir is
// chosen, if the newest Settings/backups/project-s-*.zip is missing or older
// than `backupIntervalDays` (0 disables), take one via create_backup and then
// prune to the newest `backupKeep`. The caller runs this on a background
// thread so it never blocks startup; a per-working-dir guard makes concurrent
// triggers (startup + choose_working_dir) collapse to a single run.

// Working dirs with a backup in flight; a second trigger for the same dir
// skips instead of racing a duplicate. A mutex (not a bare flag) so distinct
// working dirs never block each other.
static ACTIVE_BACKUPS: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

struct BackupGuard {
    wd: PathBuf,
}

impl BackupGuard {
    fn try_acquire(wd: &Path) -> Option<Self> {
        let mut active = ACTIVE_BACKUPS.lock().unwrap_or_else(|e| e.into_inner());
        if active.contains(wd) {
            return None;
        }
        active.insert(wd.to_path_buf());
        Some(BackupGuard { wd: wd.to_path_buf() })
    }
}

impl Drop for BackupGuard {
    fn drop(&mut self) {
        let mut active = ACTIVE_BACKUPS.lock().unwrap_or_else(|e| e.into_inner());
        active.remove(&self.wd);
    }
}

// Backups named `project-s-*.zip`, oldest first by mtime (the filename
// timestamp has only second resolution, so mtime is the reliable sort key).
fn list_backups(wd: &Path) -> Vec<PathBuf> {
    let mut backups = Vec::new();
    if let Ok(entries) = fs::read_dir(paths::backups_dir(wd)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
                if name.starts_with("project-s-") && name.ends_with(".zip") {
                    backups.push(path);
                }
            }
        }
    }
    backups.sort_by_key(|p| mtime(p));
    backups
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn backup_cutoff(interval_days: u32) -> SystemTime {
    SystemTime::now() - Duration::from_secs(u64::from(interval_days) * 86_400)
}

// A backup is due when auto-backup is enabled and there is no backup yet or
// the newest one predates the interval. `backups` must be list_backups output.
fn due_from_backups(backups: &[PathBuf], interval_days: u32) -> bool {
    if interval_days == 0 {
        return false;
    }
    match backups.last() {
        None => true,
        Some(last) => mtime(last) < backup_cutoff(interval_days),
    }
}

// Runs one auto-backup pass for `wd`. Returns Some(info) when a fresh backup
// was written, None when disabled/not due/another run is in flight. Errors
// from the backup or retention are propagated for the caller to log.
pub fn auto_backup(wd: &Path, conn: &Connection) -> Result<Option<BackupInfo>> {
    let cfg = read_app_config(wd)?;
    if cfg.backup_interval_days == 0 {
        return Ok(None);
    }
    if !due_from_backups(&list_backups(wd), cfg.backup_interval_days) {
        return Ok(None);
    }
    let _guard = match BackupGuard::try_acquire(wd) {
        Some(g) => g,
        None => return Ok(None),
    };
    let info = create_backup(wd, conn, None)?;
    apply_retention(wd, cfg.backup_keep, &PathBuf::from(&info.path));
    Ok(Some(info))
}

// Keep the newest `keep` backups, delete the rest. The just-created backup is
// never removed even when `keep` is 0.
fn apply_retention(wd: &Path, keep: u32, just_created: &Path) {
    let keep = keep.max(1) as usize;
    let mut backups = list_backups(wd);
    if backups.len() <= keep {
        return;
    }
    let excess = backups.len() - keep;
    for path in backups.drain(..excess) {
        if path == just_created {
            continue;
        }
        let _ = fs::remove_file(path);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    pub last_backup_at: Option<String>,
    pub interval_days: u32,
    pub keep: u32,
    pub count: usize,
    pub due: bool,
}

// Snapshot of the auto-backup state for the settings UI: when the last backup
// was taken (RFC3339 UTC), the configured interval/keep, how many backups are
// on disk, and whether one is due right now.
pub fn backup_status(wd: &Path) -> Result<BackupStatus> {
    let cfg = read_app_config(wd)?;
    let backups = list_backups(wd);
    let last_backup_at = backups.last().map(|p| rfc3339_utc(mtime(p)));
    Ok(BackupStatus {
        last_backup_at,
        interval_days: cfg.backup_interval_days,
        keep: cfg.backup_keep,
        count: backups.len(),
        due: due_from_backups(&backups, cfg.backup_interval_days),
    })
}

fn rfc3339_utc(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::onboarding::temp_test_dir;

    fn scaffold(tag: &str) -> PathBuf {
        let wd = temp_test_dir(tag);
        for dir in paths::scaffold_dirs(&wd) {
            fs::create_dir_all(dir).unwrap();
        }
        let project = paths::projects_dir(&wd).join("example-ru");
        fs::create_dir_all(project.join("sub")).unwrap();
        fs::write(project.join("a.md"), "alpha").unwrap();
        fs::write(project.join("sub/b.txt"), "beta-content").unwrap();
        let notes = paths::notes_dir(&wd).join("sub");
        fs::create_dir_all(&notes).unwrap();
        fs::write(paths::notes_dir(&wd).join("foo.md"), "note-content").unwrap();
        fs::write(notes.join("bar.md"), "nested-note").unwrap();
        wd
    }

    #[test]
    fn backup_zip_holds_db_snapshot_project_and_notes_files_at_default_path() {
        let wd = scaffold("backup");
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let info = create_backup(&wd, &conn, None).unwrap();

        let path = PathBuf::from(&info.path);
        assert!(path.starts_with(paths::backups_dir(&wd)));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("project-s-") && name.ends_with(".zip"));
        assert!(info.entries >= 5);
        assert!(info.bytes > 0);

        let file = File::open(&path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        assert!(names.contains(&"project-s.db".to_string()));
        assert!(names.contains(&"Projects/example-ru/a.md".to_string()));
        assert!(names.contains(&"Projects/example-ru/sub/b.txt".to_string()));
        assert!(names.contains(&"Notes/foo.md".to_string()));
        assert!(names.contains(&"Notes/sub/bar.md".to_string()));
        // tmp is NOT part of the backup (SPEC 3.6).
        assert!(!names.iter().any(|n| n.starts_with("Harness")));

        // The DB entry opens as a real SQLite file.
        let mut entry = archive.by_name("project-s.db").unwrap();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).unwrap();
        assert!(!buf.is_empty());
        let check = unique_temp_db("test-check");
        fs::write(&check, &buf).unwrap();
        db::ensure_vec_extension();
        let c = Connection::open(&check).unwrap();
        let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        drop(c);
        let _ = fs::remove_file(check);
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn backup_to_explicit_dest_path() {
        let wd = scaffold("backup-dest");
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let out = temp_test_dir("backup-dest-out");
        let dest = out.join("nested").join("chosen.zip");
        let info = create_backup(&wd, &conn, Some(&dest)).unwrap();
        assert_eq!(PathBuf::from(&info.path), dest);
        assert!(dest.is_file());
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn backup_with_empty_notes_vault_is_fine() {
        let wd = temp_test_dir("backup-empty-notes");
        for dir in paths::scaffold_dirs(&wd) {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(paths::projects_dir(&wd).join("a.md"), "alpha").unwrap();
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let info = create_backup(&wd, &conn, None).unwrap();

        let archive = zip::ZipArchive::new(File::open(&info.path).unwrap()).unwrap();
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        assert!(names.contains(&"project-s.db".to_string()));
        assert!(names.contains(&"Projects/a.md".to_string()));
        assert!(!names.iter().any(|n| n.starts_with("Notes")));
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn clear_tmp_removes_contents_but_keeps_the_folder() {
        let wd = scaffold("tmp-clear");
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("a.bin"), "12345").unwrap();
        fs::write(tmp.join("sub/b.bin"), "6789").unwrap();

        let stats = clear_tmp(&wd).unwrap();
        assert_eq!(stats.removed_files, 2);
        assert_eq!(stats.freed_bytes, 9);
        assert!(tmp.is_dir());
        assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn cleanup_old_tmp_respects_age_and_zero_disables() {
        let wd = scaffold("tmp-age");
        let tmp = paths::harness_tmp_dir(&wd);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        let old = tmp.join("old.bin");
        let old_in_sub = tmp.join("sub/old2.bin");
        let fresh = tmp.join("fresh.bin");
        fs::write(&old, "old-content").unwrap(); // 11 bytes
        fs::write(&old_in_sub, "old2").unwrap(); // 4 bytes
        fs::write(&fresh, "fresh").unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(10 * 86_400);
        for path in [&old, &old_in_sub] {
            File::options().write(true).open(path).unwrap().set_modified(long_ago).unwrap();
        }

        // 0 disables auto-purge entirely.
        assert_eq!(cleanup_old_tmp(&wd, 0).unwrap().removed_files, 0);
        assert!(old.is_file());

        let stats = cleanup_old_tmp(&wd, 7).unwrap();
        assert_eq!((stats.removed_files, stats.freed_bytes), (2, 15));
        assert!(!old.exists());
        assert!(fresh.is_file());
        // The emptied subfolder is pruned; the tmp root is kept.
        assert!(!tmp.join("sub").exists());
        assert!(tmp.is_dir());
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn autostart_choice_round_trips_through_app_json() {
        let wd = scaffold("autostart");
        config::ensure_defaults(&wd).unwrap();
        assert!(!autostart_choice(&wd)); // default off (SPEC 8)
        config::write_config(&wd, ConfigKind::App, serde_json::json!({ "customFlag": true })).unwrap();
        set_autostart_choice(&wd, true).unwrap();
        assert!(autostart_choice(&wd));
        let raw = config::read_config(&wd, ConfigKind::App).unwrap();
        assert_eq!(raw["autostart"], true);
        // Unknown app.json keys survive the autostart rewrite.
        assert_eq!(raw["customFlag"], true);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn apply_autostart_guards_backend_errors_without_panicking() {
        let failing = |_enabled: bool| Err("autostart backend unavailable".to_string());
        assert!(matches!(
            apply_autostart(&failing, true),
            Err(AppError::Io(_))
        ));
        let ok = |_enabled: bool| Ok(());
        assert!(apply_autostart(&ok, false).is_ok());
    }

    // --- automatic backups (SPEC 3.6) ---

    fn set_backup_cfg(wd: &Path, interval: u32, keep: u32) {
        config::write_config(
            wd,
            ConfigKind::App,
            serde_json::json!({ "backupIntervalDays": interval, "backupKeep": keep }),
        )
        .unwrap();
    }

    // A placeholder archive with an mtime `age_days` in the past; auto_backup
    // only reads names/mtimes, so the content never has to be a real zip.
    fn seed_backup(wd: &Path, name: &str, age_days: u64) {
        let dir = paths::backups_dir(wd);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, "seed").unwrap();
        let mtime = SystemTime::now() - Duration::from_secs(age_days * 86_400);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn backup_names(wd: &Path) -> Vec<String> {
        list_backups(wd)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn auto_backup_zero_interval_is_disabled_and_creates_nothing() {
        let wd = scaffold("auto-backup-off");
        set_backup_cfg(&wd, 0, 7);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        assert!(auto_backup(&wd, &conn).unwrap().is_none());
        assert_eq!(list_backups(&wd).len(), 0);
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_backup_first_run_creates_one() {
        let wd = scaffold("auto-backup-first");
        set_backup_cfg(&wd, 1, 7);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        let info = auto_backup(&wd, &conn).unwrap();
        assert!(info.is_some());
        assert_eq!(list_backups(&wd).len(), 1);
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_backup_second_run_within_interval_does_not_duplicate() {
        let wd = scaffold("auto-backup-dup");
        set_backup_cfg(&wd, 1, 7);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        assert!(auto_backup(&wd, &conn).unwrap().is_some());
        assert!(auto_backup(&wd, &conn).unwrap().is_none());
        assert_eq!(list_backups(&wd).len(), 1);
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_backup_old_backup_triggers_a_new_one() {
        let wd = scaffold("auto-backup-old");
        set_backup_cfg(&wd, 1, 7);
        seed_backup(&wd, "project-s-old.zip", 10);
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        assert!(auto_backup(&wd, &conn).unwrap().is_some());
        // keep=7: the seeded old archive stays, a fresh one is added.
        assert_eq!(list_backups(&wd).len(), 2);
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn auto_backup_retention_keeps_newest_and_deletes_older() {
        let wd = scaffold("auto-backup-keep");
        set_backup_cfg(&wd, 1, 2);
        // Five seeded archives, oldest (10 d) to newest (6 d) by mtime.
        for (i, age) in [10_u64, 9, 8, 7, 6].iter().enumerate() {
            seed_backup(&wd, &format!("project-s-old{i}.zip"), *age);
        }
        let conn = db::open(&paths::db_path(&wd)).unwrap();
        assert!(auto_backup(&wd, &conn).unwrap().is_some());
        let names = backup_names(&wd);
        // keep=2: the just-created archive plus the newest seeded (old4).
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "project-s-old4.zip"));
        for i in 0..4 {
            assert!(!names.iter().any(|n| *n == format!("project-s-old{i}.zip")));
        }
        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn backup_status_reports_fields_and_due() {
        let wd = scaffold("auto-backup-status");
        set_backup_cfg(&wd, 1, 7);
        let conn = db::open(&paths::db_path(&wd)).unwrap();

        let st = backup_status(&wd).unwrap();
        assert_eq!((st.interval_days, st.keep, st.count), (1, 7, 0));
        assert!(st.last_backup_at.is_none());
        assert!(st.due);

        auto_backup(&wd, &conn).unwrap();
        let st = backup_status(&wd).unwrap();
        assert_eq!(st.count, 1);
        assert!(st.last_backup_at.is_some());
        assert!(!st.due);

        // Disabled (interval 0): never due, existing backups still counted.
        set_backup_cfg(&wd, 0, 7);
        let st = backup_status(&wd).unwrap();
        assert_eq!(st.interval_days, 0);
        assert_eq!(st.count, 1);
        assert!(!st.due);

        drop(conn);
        fs::remove_dir_all(&wd).unwrap();
    }
}
