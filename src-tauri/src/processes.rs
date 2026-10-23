// Background process control (the domain behind agent/process_tools.rs):
// long-running commands (dev servers, installs) start detached, their
// combined stdout+stderr is appended to a log under
// Harness/tmp/processes/, and they are tracked so the row always reflects a
// real process. How a command is built, probed and signalled is platform
// detail and lives in platform::process (Unix: sh -c in its own process
// group, kill -0 / kill -TERM -<pgid> / kill -KILL -<pgid>; Windows: cmd /C
// with a hidden console, tasklist to probe, taskkill /T /F to stop the
// tree). Stopping keeps the same ~2 s grace between the soft and hard kill.
//
// Spawned children are kept in an in-process registry so reconcile can
// reap them (try_wait) before any liveness probe: an unreaped exited child
// stays a zombie, and a probe on a zombie still succeeds. Rows left by a
// previous app run are not our children — their processes were reparented
// to launchd and keep running; startup reconcile only updates their
// status, it never kills them.
use crate::error::{AppError, Result};
use crate::sync_util::lock_recover;
use crate::paths;
use crate::platform;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_LOG_LINES: usize = 200;
pub const DEFAULT_LOG_CHARS: usize = 20_000;

// Only the tail of a log file is read; a runaway dev server can grow it
// without bound.
const LOG_TAIL_BYTES: usize = 1024 * 1024;
const STOP_GRACE: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(100);

// A tracked spawned child; tests substitute a fake instead of real processes.
trait TrackedChild {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> std::io::Result<()>;
}

impl TrackedChild for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
}

// pid is unique per boot, so it keys the registry across test databases.
// (Lazy: HashMap::new is not const, so the map materializes on first use.)
struct ProcessRegistry<C: TrackedChild = Child> {
    children: Mutex<Option<HashMap<u32, C>>>,
}

impl<C: TrackedChild> ProcessRegistry<C> {
    const fn new() -> Self {
        Self {
            children: Mutex::new(None),
        }
    }

    fn with_children<T>(&self, f: impl FnOnce(&mut HashMap<u32, C>) -> T) -> T {
        let mut guard = lock_recover(&self.children);
        f(guard.get_or_insert_with(HashMap::new))
    }

    fn register(&self, child: C) {
        let pid = child.id();
        self.with_children(|children| {
            children.insert(pid, child);
        });
    }

    fn reap(&self, pid: u32) -> Option<ExitStatus> {
        self.with_children(|children| {
            if let Some(child) = children.get_mut(&pid) {
                if let Ok(Some(status)) = child.try_wait() {
                    children.remove(&pid);
                    return Some(status);
                }
            }
            None
        })
    }

    fn contains(&self, pid: u32) -> bool {
        self.with_children(|children| children.contains_key(&pid))
    }

    #[allow(dead_code)] // runtime exit hook; exercised by cfg(test), not wired to app exit yet
    fn shutdown(&self) {
        let drained: Vec<C> = {
            let mut guard = lock_recover(&self.children);
            guard.take().map_or_else(Vec::new, |children| children.into_values().collect())
        };
        for mut child in drained {
            let _ = child.kill();
        }
    }
}

static REGISTRY: ProcessRegistry = ProcessRegistry::new();

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessDto {
    pub id: i64,
    pub name: Option<String>,
    pub pid: i64,
    pub pgid: i64,
    pub command: String,
    pub cwd: Option<String>,
    pub log_path: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

const COLS: &str = "id, name, pid, pgid, command, cwd, log_path, status, exit_code, created_at, updated_at";

fn dto_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessDto> {
    Ok(ProcessDto {
        id: row.get(0)?,
        name: row.get(1)?,
        pid: row.get(2)?,
        pgid: row.get(3)?,
        command: row.get(4)?,
        cwd: row.get(5)?,
        log_path: row.get(6)?,
        status: row.get(7)?,
        exit_code: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn get(conn: &Connection, id: i64) -> Result<Option<ProcessDto>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM processes WHERE id = ?1"),
            params![id],
            dto_from_row,
        )
        .optional()?)
}

fn get_required(conn: &Connection, id: i64) -> Result<ProcessDto> {
    get(conn, id)?.ok_or_else(|| AppError::Db(format!("process {id} vanished")))
}

// Filename-safe slug for the log file: ascii alphanumerics, dashes between
// the rest, capped so the file name stays short.
fn slug(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(40).collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "proc".to_string()
    } else {
        out
    }
}

// Reaps a spawned child that has already exited and returns its status;
// children from a previous app run are not in the registry.
fn reap(pid: u32) -> Option<ExitStatus> {
    REGISTRY.reap(pid)
}

fn is_our_child(pid: i64) -> bool {
    REGISTRY.contains(pid as u32)
}

// Group liveness for the stop poll: reap first, so an exited-but-unreaped
// group leader (zombie) does not keep the group looking alive.
fn group_alive(pid: i64, pgid: i64) -> bool {
    reap(pid as u32);
    let target = platform::process::liveness_target(pid, pgid);
    platform::process::kill_alive(&target)
}

// Brings one 'running' row in line with reality: reap our own exited
// child (exit code 0 → 'exited', non-zero → 'error'), otherwise probe the
// pid with kill -0 — a dead foreign pid becomes 'exited'.
fn reconcile_row(conn: &Connection, row: &ProcessDto) -> Result<()> {
    if let Some(status) = reap(row.pid as u32) {
        let code = status.code().unwrap_or(-1) as i64;
        let new_status = if code == 0 { "exited" } else { "error" };
        conn.execute(
            "UPDATE processes SET status = ?1, exit_code = ?2,
                 updated_at = datetime('now','localtime')
             WHERE id = ?3 AND status = 'running'",
            params![new_status, code, row.id],
        )?;
        return Ok(());
    }
    if is_our_child(row.pid) {
        return Ok(());
    }
    if !platform::process::kill_alive(&row.pid.to_string()) {
        conn.execute(
            "UPDATE processes SET status = 'exited',
                 updated_at = datetime('now','localtime')
             WHERE id = ?1 AND status = 'running'",
            params![row.id],
        )?;
    }
    Ok(())
}

// Reconciles every 'running' row; runs on app start (stale rows from a
// previous run are never killed) and before process_list/process_logs.
pub fn reconcile(conn: &Connection) -> Result<()> {
    let rows: Vec<ProcessDto> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM processes WHERE status = 'running' ORDER BY id"
        ))?;
        let mapped = stmt.query_map([], dto_from_row)?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for row in rows {
        reconcile_row(conn, &row)?;
    }
    Ok(())
}

// Starts a detached background command (platform::process::background_command),
// with stdout+stderr appended to <id>-<slug>.log under Harness/tmp/processes/
// and TMPDIR/TEMP/TMP pointed at Harness/tmp (never the system /tmp). The
// child is not waited on; it is registered for later reaping.
pub fn start(
    conn: &Connection,
    wd: &Path,
    command: &str,
    cwd: Option<&Path>,
    name: Option<&str>,
) -> Result<ProcessDto> {
    if command.trim().is_empty() {
        return Err(AppError::Invalid("command is empty".into()));
    }
    let cwd = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::harness_dir(wd));
    fs::create_dir_all(&cwd)?;
    let tmp = paths::harness_tmp_dir(wd);
    fs::create_dir_all(&tmp)?;
    let log_dir = paths::harness_processes_dir(wd);
    fs::create_dir_all(&log_dir)?;

    conn.execute(
        "INSERT INTO processes (name, pid, pgid, command, cwd, log_path, status, created_at, updated_at)
         VALUES (?1, 0, 0, ?2, ?3, '', 'running', datetime('now','localtime'), datetime('now','localtime'))",
        params![name, command, cwd.display().to_string()],
    )?;
    let id = conn.last_insert_rowid();
    let log_path = log_dir.join(format!("{}-{}.log", id, slug(name.unwrap_or(command))));

    let log = OpenOptions::new().append(true).create(true).open(&log_path)?;
    let mut cmd = platform::process::background_command(command);
    cmd.current_dir(&cwd)
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    let spawned = cmd.spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(e) => {
            conn.execute("DELETE FROM processes WHERE id = ?1", params![id])?;
            return Err(AppError::Io(format!("spawn: {e}")));
        }
    };
    // On Unix the child leads a new process group (platform::process), so
    // the stored pgid equals the pid; Windows has no groups and stores the
    // pid twice, which is what taskkill /T then works from.
    let pid = child.id() as i64;
    conn.execute(
        "UPDATE processes SET pid = ?1, pgid = ?2, log_path = ?3,
             updated_at = datetime('now','localtime')
         WHERE id = ?4",
        params![pid, pid, log_path.display().to_string(), id],
    )?;
    REGISTRY.register(child);
    get_required(conn, id)
}

// One row reconciled to reality, for process_logs.
pub fn get_reconciled(conn: &Connection, id: i64) -> Result<ProcessDto> {
    let row = get(conn, id)?.ok_or_else(|| AppError::Invalid(format!("process {id} not found")))?;
    if row.status == "running" {
        reconcile_row(conn, &row)?;
        return get_required(conn, id);
    }
    Ok(row)
}

// Last `lines` lines of the log, cut to the last `max_chars` characters;
// a missing file (purged tmp) reads as empty.
pub fn tail_log(path: &Path, lines: usize, max_chars: usize) -> Result<String> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.into()),
    };
    let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    let text = String::from_utf8_lossy(&bytes[start..]);
    let mut kept: Vec<&str> = text.lines().rev().take(lines).collect();
    kept.reverse();
    let mut out = kept.join("\n");
    let count = out.chars().count();
    if count > max_chars {
        out = out.chars().skip(count - max_chars).collect();
    }
    Ok(out)
}

// TERM to the whole group, wait ~2 s, KILL if it is still alive; the row
// becomes 'stopped'. A row that is no longer running is returned as is.
pub fn stop(conn: &Connection, id: i64) -> Result<ProcessDto> {
    let row = get(conn, id)?.ok_or_else(|| AppError::Invalid(format!("process {id} not found")))?;
    if row.status != "running" {
        return Ok(row);
    }
    // The process may have exited since the row was read: reconcile first
    // so an already-dead process is reported with its real status.
    reconcile_row(conn, &row)?;
    let row = get_required(conn, id)?;
    if row.status != "running" {
        return Ok(row);
    }
    platform::process::signal_group(row.pid, row.pgid, "-TERM");
    let deadline = Instant::now() + STOP_GRACE;
    while group_alive(row.pid, row.pgid) && Instant::now() < deadline {
        std::thread::sleep(STOP_POLL);
    }
    if group_alive(row.pid, row.pgid) {
        platform::process::signal_group(row.pid, row.pgid, "-KILL");
        let deadline = Instant::now() + STOP_GRACE;
        while group_alive(row.pid, row.pgid) && Instant::now() < deadline {
            std::thread::sleep(STOP_POLL);
        }
    }
    conn.execute(
        "UPDATE processes SET status = 'stopped',
             updated_at = datetime('now','localtime')
         WHERE id = ?1 AND status = 'running'",
        params![id],
    )?;
    get_required(conn, id)
}

// All rows, reconciled first; newest first for the tool output.
pub fn list(conn: &Connection) -> Result<Vec<ProcessDto>> {
    reconcile(conn)?;
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM processes ORDER BY id DESC"))?;
    let mapped = stmt.query_map([], dto_from_row)?;
    Ok(mapped.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FakeChild {
        pid: u32,
        exited: Arc<AtomicBool>,
        kills: Arc<AtomicUsize>,
    }

    fn fake_child(pid: u32, kills: &Arc<AtomicUsize>) -> (FakeChild, Arc<AtomicBool>) {
        let exited = Arc::new(AtomicBool::new(false));
        (
            FakeChild {
                pid,
                exited: Arc::clone(&exited),
                kills: Arc::clone(kills),
            },
            exited,
        )
    }

    impl TrackedChild for FakeChild {
        fn id(&self) -> u32 {
            self.pid
        }
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            if self.exited.load(Ordering::SeqCst) {
                Ok(Some(ExitStatus::from_raw(0)))
            } else {
                Ok(None)
            }
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn registry_register_reap_removes_exited_child() {
        let reg = ProcessRegistry::<FakeChild>::new();
        let kills = Arc::new(AtomicUsize::new(0));
        let (child, exited) = fake_child(11, &kills);
        reg.register(child);
        assert!(reg.contains(11));
        assert!(reg.reap(11).is_none());
        exited.store(true, Ordering::SeqCst);
        let status = reg.reap(11).expect("exited child should reap");
        assert_eq!(status.code(), Some(0));
        assert!(!reg.contains(11));
        assert!(reg.reap(11).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reap_drops_dead_children_without_touching_live_ones() {
        let reg = ProcessRegistry::<FakeChild>::new();
        let kills = Arc::new(AtomicUsize::new(0));
        let (dead, dead_exited) = fake_child(1, &kills);
        let (live, _) = fake_child(2, &kills);
        reg.register(dead);
        reg.register(live);
        dead_exited.store(true, Ordering::SeqCst);
        assert!(reg.reap(1).is_some());
        assert!(reg.reap(2).is_none());
        assert!(!reg.contains(1));
        assert!(reg.contains(2));
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shutdown_kills_all_once_and_is_idempotent() {
        let reg = ProcessRegistry::<FakeChild>::new();
        let kills = Arc::new(AtomicUsize::new(0));
        for pid in 1..=3u32 {
            let (child, _) = fake_child(pid, &kills);
            reg.register(child);
        }
        reg.shutdown();
        reg.shutdown();
        assert_eq!(kills.load(Ordering::SeqCst), 3);
        for pid in 1..=3u32 {
            assert!(!reg.contains(pid));
        }
    }

    #[test]
    fn concurrent_registry_access_serializes_registrations() {
        let reg = Arc::new(ProcessRegistry::<FakeChild>::new());
        let mut handles = Vec::new();
        for pid in 1..=8u32 {
            let reg = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || {
                let kills = Arc::new(AtomicUsize::new(0));
                let (child, _) = fake_child(pid, &kills);
                reg.register(child);
                reg.reap(pid);
                reg.contains(pid)
            }));
        }
        for handle in handles {
            assert!(handle.join().unwrap());
        }
        for pid in 1..=8u32 {
            assert!(reg.contains(pid));
        }
    }

    #[test]
    fn slug_keeps_filenames_safe() {
        assert_eq!(slug("npm run dev"), "npm-run-dev");
        assert_eq!(slug("  python3 -m http.server 8731  "), "python3-m-http-server-8731");
        assert_eq!(slug("??"), "proc");
        assert_eq!(slug("привет мир"), "proc");
        assert_eq!(slug(&"a".repeat(100)).len(), 40);
    }

    #[test]
    fn tail_log_caps_lines_and_chars_and_tolerates_missing_file() {
        let dir = std::env::temp_dir().join(format!("project-s-taillog-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.log");
        fs::write(&path, "l1\nl2\nl3\nl4").unwrap();
        assert_eq!(tail_log(&path, 2, 1000).unwrap(), "l3\nl4");
        assert_eq!(tail_log(&path, 10, 4).unwrap(), "3\nl4");
        assert_eq!(tail_log(&dir.join("missing.log"), 5, 5).unwrap(), "");
        fs::remove_dir_all(&dir).unwrap();
    }
}
