// Migrations v31-v40: task model update, due time, wall-clock timer, project
// file view, skill proposals, style profile, chat pin, project archive,
// schedule unread stamp, and the builtin project. SQL strings and order are
// unchanged from the original schema.rs.
use crate::error::Result;
use rusqlite::Connection;

// Task model update (SPEC task model slice): a single is_priority flag
// replaces the four-level priority in the DTO/tools/export (the old priority
// column stays in the table for data safety, just unused), and per-project
// kanban column names as a JSON array of 3 strings. Guarded per column
// (v14 recipe) so a re-run after an interrupted v31 does not re-ALTER; the
// backfill UPDATE is idempotent.
const MIGRATION_31_TASK_IS_PRIORITY: &str =
    "ALTER TABLE tasks ADD COLUMN is_priority INTEGER NOT NULL DEFAULT 0";
const MIGRATION_31_TASK_IS_PRIORITY_BACKFILL: &str =
    "UPDATE tasks SET is_priority = 1 WHERE priority IN ('high','urgent')";
const MIGRATION_31_PROJECT_TASK_COLUMNS: &str = "ALTER TABLE projects ADD COLUMN task_columns TEXT NOT NULL DEFAULT '[\"Новые\",\"В работе\",\"Готово\"]'";

// Task deadline time (SPEC 4.3): HH:MM next to due_date; NOT NULL DEFAULT
// backfills every existing row with 07:00, so tasks that already have a due
// date keep showing 07:00. Format is validated in Rust (tasks.rs), like
// due_date has no CHECK either. Guarded per column (v14 recipe).
const MIGRATION_32_TASK_DUE_TIME: &str =
    "ALTER TABLE tasks ADD COLUMN due_time TEXT NOT NULL DEFAULT '07:00'";

// Task wall-clock timer (SPEC 4.3): accumulated spent milliseconds plus the
// active start instant. Epoch ms (UTC) keeps date/time strings out of the
// timer: the count keeps running while the app is closed. Backfill decision
// (documented in SPEC): tasks already in 'progress' start counting at
// migration time, so no task claims time spent before the feature existed.
// Guarded per column (v14 recipe); the backfill UPDATE is idempotent.
const MIGRATION_33_TASK_SPENT_MS: &str = "ALTER TABLE tasks ADD COLUMN spent_ms INTEGER NOT NULL DEFAULT 0";
const MIGRATION_33_TASK_STARTED_AT: &str = "ALTER TABLE tasks ADD COLUMN started_at INTEGER";
const MIGRATION_33_TASK_STARTED_AT_BACKFILL: &str =
    "UPDATE tasks SET started_at = CAST(strftime('%s','now') AS INTEGER) * 1000 WHERE status = 'progress' AND started_at IS NULL";

// Per-project file view mode (SPEC 4.2): "list" | "grid". NOT NULL DEFAULT
// backfills existing rows with "list" (the mode the UI had before the feature
// existed); the CHECK keeps the column closed and the default satisfies it,
// so the guarded ADD COLUMN is valid. Values are also normalized in Rust
// (projects.rs). Guarded per column (v14 recipe).
const MIGRATION_34_PROJECT_FILES_VIEW: &str =
    "ALTER TABLE projects ADD COLUMN files_view TEXT NOT NULL DEFAULT 'list' CHECK (files_view IN ('list','grid'))";

// Repeat detector for skill proposals (SPEC 6.2 Q20): one row per complex-run
// signature (normalized user request + sorted tool names + tool count).
// `occurrences` counts complex completed runs; `status` drives the proposal
// lifecycle: 'seen' (counted, not yet proposed) → 'proposed' (the one-time
// chat proposal was posted; also suppresses any further proposal, which is
// how a declined or ignored proposal stays silent) → 'skipped' (a skill with
// the candidate name already exists). The scope ids point at the chat of the
// latest occurrence; no FK: a deleted chat must not erase repeat statistics.
const MIGRATION_35_SKILL_PROPOSALS: &str = "
CREATE TABLE IF NOT EXISTS skill_proposals (
    signature TEXT PRIMARY KEY,
    occurrences INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'seen' CHECK (status IN ('seen','proposed','skipped')),
    last_text TEXT NOT NULL,
    tool_names TEXT NOT NULL,
    tool_count INTEGER NOT NULL,
    chat_id INTEGER,
    project_id INTEGER,
    session_id INTEGER,
    updated_at TEXT NOT NULL
);
";

// Style profile (SPEC 6.6 Q-26): one global row (id = 1) of decayed counters
// derived from the user's own messages only — address (ты/вы), emoji, profanity,
// word and message counts. Shared by every chat and session, survives restarts.
// The agent's style instruction is derived from these counters and injected into
// every system prompt through the [Memory] block.
const MIGRATION_36_STYLE_PROFILE: &str = "
CREATE TABLE IF NOT EXISTS style_profile (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    informal REAL NOT NULL DEFAULT 0,
    formal REAL NOT NULL DEFAULT 0,
    emoji REAL NOT NULL DEFAULT 0,
    profanity REAL NOT NULL DEFAULT 0,
    words REAL NOT NULL DEFAULT 0,
    messages REAL NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);
";

// Persistent pinning of Harness chats (Q.md pin slice): a nullable pinned_at
// on the kind='harness' chat row is the pin stamp (NULL = unpinned). Only
// Harness sessions are pinned; project and notes chats leave it NULL. Guarded
// per column (v14 recipe) so a re-run after an interrupted v37 does not
// re-ALTER.
const MIGRATION_37_CHAT_PINNED_AT: &str = "ALTER TABLE chats ADD COLUMN pinned_at TEXT";

const MIGRATION_38_PROJECT_ARCHIVED_AT: &str = "ALTER TABLE projects ADD COLUMN archived_at TEXT";

// Q.md item 7: an unread schedule result stamp (NULL = read). Set when a
// scheduled agent run finishes, cleared when the schedule's chat is opened.
const MIGRATION_39_SCHEDULE_UNREAD_AT: &str = "ALTER TABLE schedules ADD COLUMN unread_at TEXT";

pub(super) fn migrate_v31(conn: &Connection) -> Result<()> {
    // Guarded per column (v14 recipe) so a re-run after an interrupted
    // v31 does not re-ALTER.
    let has_is_priority: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('tasks') WHERE name = 'is_priority'",
        [],
        |r| r.get(0),
    )?;
    if has_is_priority == 0 {
        conn.execute(MIGRATION_31_TASK_IS_PRIORITY, [])?;
    }
    conn.execute(MIGRATION_31_TASK_IS_PRIORITY_BACKFILL, [])?;
    let has_task_columns: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'task_columns'",
        [],
        |r| r.get(0),
    )?;
    if has_task_columns == 0 {
        conn.execute(MIGRATION_31_PROJECT_TASK_COLUMNS, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v32(conn: &Connection) -> Result<()> {
    // Guarded per column (v14 recipe) so a re-run after an interrupted
    // v32 does not re-ALTER; the NOT NULL DEFAULT backfills existing rows.
    let has_due_time: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('tasks') WHERE name = 'due_time'",
        [],
        |r| r.get(0),
    )?;
    if has_due_time == 0 {
        conn.execute(MIGRATION_32_TASK_DUE_TIME, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v33(conn: &Connection) -> Result<()> {
    // Guarded per column (v14 recipe) so a re-run after an interrupted
    // v33 does not re-ALTER; the backfill only touches rows that never
    // got a start stamp, so it stays idempotent across re-runs.
    let has_spent_ms: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('tasks') WHERE name = 'spent_ms'",
        [],
        |r| r.get(0),
    )?;
    if has_spent_ms == 0 {
        conn.execute(MIGRATION_33_TASK_SPENT_MS, [])?;
    }
    let has_started_at: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('tasks') WHERE name = 'started_at'",
        [],
        |r| r.get(0),
    )?;
    if has_started_at == 0 {
        conn.execute(MIGRATION_33_TASK_STARTED_AT, [])?;
    }
    conn.execute(MIGRATION_33_TASK_STARTED_AT_BACKFILL, [])?;
    Ok(())
}

pub(super) fn migrate_v34(conn: &Connection) -> Result<()> {
    // Guarded per column (v14 recipe) so a re-run after an interrupted
    // v34 does not re-ALTER.
    let has_files_view: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'files_view'",
        [],
        |r| r.get(0),
    )?;
    if has_files_view == 0 {
        conn.execute(MIGRATION_34_PROJECT_FILES_VIEW, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v35(conn: &Connection) -> Result<()> {
    // CREATE TABLE IF NOT EXISTS: idempotent across interrupted re-runs.
    conn.execute_batch(MIGRATION_35_SKILL_PROPOSALS)?;
    Ok(())
}

pub(super) fn migrate_v36(conn: &Connection) -> Result<()> {
    // CREATE TABLE IF NOT EXISTS: idempotent across interrupted re-runs.
    conn.execute_batch(MIGRATION_36_STYLE_PROFILE)?;
    Ok(())
}

pub(super) fn migrate_v37(conn: &Connection) -> Result<()> {
    // Guarded per column (v14 recipe) so a re-run after an interrupted v37
    // does not re-ALTER.
    let has_pinned_at: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('chats') WHERE name = 'pinned_at'",
        [],
        |r| r.get(0),
    )?;
    if has_pinned_at == 0 {
        conn.execute(MIGRATION_37_CHAT_PINNED_AT, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v38(conn: &Connection) -> Result<()> {
    let has_archived_at: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'archived_at'",
        [],
        |r| r.get(0),
    )?;
    if has_archived_at == 0 {
        conn.execute(MIGRATION_38_PROJECT_ARCHIVED_AT, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v39(conn: &Connection) -> Result<()> {
    let has_unread_at: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('schedules') WHERE name = 'unread_at'",
        [],
        |r| r.get(0),
    )?;
    if has_unread_at == 0 {
        conn.execute(MIGRATION_39_SCHEDULE_UNREAD_AT, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v40(conn: &Connection) -> Result<()> {
    let has_builtin: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'builtin'",
        [],
        |r| r.get(0),
    )?;
    if has_builtin == 0 {
        conn.execute("ALTER TABLE projects ADD COLUMN builtin INTEGER NOT NULL DEFAULT 0", [])?;
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS uniq_projects_builtin ON projects(builtin) WHERE builtin = 1",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::schema::SCHEMA_VERSION;
    use rusqlite::params;

    #[test]
    fn migration_v31_backfills_is_priority_and_adds_default_task_columns() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v31 database: drop both columns and roll the version
        // back, with tasks carrying every old priority value.
        conn.execute("ALTER TABLE tasks DROP COLUMN is_priority", [])
            .unwrap();
        conn.execute("ALTER TABLE projects DROP COLUMN task_columns", [])
            .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        for (title, priority) in [("hi", "high"), ("ur", "urgent"), ("lo", "low"), ("no", "normal")] {
            conn.execute(
                "INSERT INTO tasks (project_id, title, created_at, priority) VALUES (1, ?1, '2026-01-01', ?2)",
                params![title, priority],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 30).unwrap();

        run_migrations(&conn).unwrap();

        // high/urgent became priority, low/normal stayed plain.
        let flags: Vec<(String, i64)> = {
            let mut stmt = conn.prepare("SELECT title, is_priority FROM tasks ORDER BY id").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            flags,
            vec![
                ("hi".to_owned(), 1),
                ("ur".to_owned(), 1),
                ("lo".to_owned(), 0),
                ("no".to_owned(), 0),
            ]
        );

        // New projects and tasks get the defaults; the column names are the
        // shipped Russian trio as a JSON array of 3 strings.
        let columns: String = conn
            .query_row("SELECT task_columns FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(columns, "[\"Новые\",\"В работе\",\"Готово\"]");
        conn.execute(
            "INSERT INTO tasks (project_id, title, created_at) VALUES (1, 'fresh', '2026-01-01')",
            [],
        )
        .unwrap();
        let fresh: i64 = conn
            .query_row("SELECT is_priority FROM tasks WHERE title = 'fresh'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fresh, 0);

        // Guarded ALTER: a v30 database whose columns already exist is repaired
        // without re-ALTER errors, and the backfill stays idempotent.
        conn.pragma_update(None, "user_version", 30).unwrap();
        run_migrations(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let hi: i64 = conn
            .query_row("SELECT is_priority FROM tasks WHERE title = 'hi'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hi, 1, "re-run must not demote backfilled flags");
    }

    #[test]
    fn migration_v32_backfills_due_time_with_default_0700() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v32 database: drop the column, roll the version back,
        // add rows with and without a due date.
        conn.execute("ALTER TABLE tasks DROP COLUMN due_time", [])
            .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, created_at, due_date) VALUES (1, 'dated', '2026-01-01', '2099-01-02')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, created_at) VALUES (1, 'undated', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 31).unwrap();

        run_migrations(&conn).unwrap();

        // Every existing row got 07:00, dated and undated alike; new inserts
        // take the column default too.
        let times: Vec<(String, String)> = {
            let mut stmt = conn.prepare("SELECT title, due_time FROM tasks ORDER BY id").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            times,
            vec![
                ("dated".to_owned(), "07:00".to_owned()),
                ("undated".to_owned(), "07:00".to_owned()),
            ]
        );
        conn.execute(
            "INSERT INTO tasks (project_id, title, created_at) VALUES (1, 'fresh', '2026-01-01')",
            [],
        )
        .unwrap();
        let fresh: String = conn
            .query_row("SELECT due_time FROM tasks WHERE title = 'fresh'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fresh, "07:00");

        // Guarded ALTER: a v31 database whose column already exists is
        // repaired without re-ALTER errors.
        conn.pragma_update(None, "user_version", 31).unwrap();
        run_migrations(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v33_backfills_started_at_only_for_in_progress_tasks() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v33 database: drop both columns, roll the version
        // back, add one task per status.
        conn.execute("ALTER TABLE tasks DROP COLUMN spent_ms", [])
            .unwrap();
        conn.execute("ALTER TABLE tasks DROP COLUMN started_at", [])
            .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        for status in ["new", "progress", "done"] {
            conn.execute(
                "INSERT INTO tasks (project_id, title, status, created_at) VALUES (1, ?1, ?1, '2026-01-01')",
                params![status],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 32).unwrap();

        run_migrations(&conn).unwrap();

        let rows: Vec<(String, i64, Option<i64>)> = {
            let mut stmt = conn
                .prepare("SELECT status, spent_ms, started_at FROM tasks ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(rows.len(), 3);
        for (status, spent_ms, started_at) in &rows {
            assert_eq!(*spent_ms, 0, "accumulated time must start at zero");
            if status == "progress" {
                let started = started_at.unwrap();
                let now: i64 = conn
                    .query_row("SELECT CAST(strftime('%s','now') AS INTEGER) * 1000", [], |r| r.get(0))
                    .unwrap();
                assert!(
                    started > now - 60_000 && started <= now,
                    "in-progress tasks must start counting at migration time"
                );
            } else {
                assert_eq!(*started_at, None, "{status} tasks must have no active start");
            }
        }

        // Idempotent re-run: stamps are not moved, no re-ALTER error.
        let first: Option<i64> = conn
            .query_row("SELECT started_at FROM tasks WHERE status = 'progress'", [], |r| r.get(0))
            .unwrap();
        conn.pragma_update(None, "user_version", 32).unwrap();
        run_migrations(&conn).unwrap();
        let again: Option<i64> = conn
            .query_row("SELECT started_at FROM tasks WHERE status = 'progress'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, again, "re-run must not re-stamp the backfill");
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v34_adds_files_view_with_default_and_check() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v34 database: drop the column and roll the version back.
        conn.execute("ALTER TABLE projects DROP COLUMN files_view", [])
            .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 33).unwrap();

        run_migrations(&conn).unwrap();

        // Existing rows get the "list" default; new rows too.
        let stored: String = conn
            .query_row("SELECT files_view FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "list");
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('q', '/tmp/q', '2026-01-01')",
            [],
        )
        .unwrap();
        let fresh: String = conn
            .query_row("SELECT files_view FROM projects WHERE name = 'q'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fresh, "list");

        // CHECK keeps the domain closed: "grid" passes, anything else is rejected.
        conn.execute("UPDATE projects SET files_view = 'grid' WHERE id = 1", [])
            .unwrap();
        let bad = conn.execute("UPDATE projects SET files_view = 'bogus' WHERE id = 1", []);
        assert!(bad.is_err(), "unknown files_view accepted");
        let bad_insert = conn.execute(
            "INSERT INTO projects (name, path, created_at, files_view) VALUES ('r', '/tmp/r', '2026-01-01', 'tiles')",
            [],
        );
        assert!(bad_insert.is_err(), "unknown files_view insert accepted");

        // Guarded ALTER: a v33 database whose column already exists is
        // repaired without re-ALTER errors.
        conn.pragma_update(None, "user_version", 33).unwrap();
        run_migrations(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v35_adds_skill_proposals_with_status_domain() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // A pre-v35 database: drop the table, roll the version back, upgrade.
        conn.execute("DROP TABLE skill_proposals", []).unwrap();
        conn.pragma_update(None, "user_version", 34).unwrap();
        run_migrations(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        conn.execute(
            "INSERT INTO skill_proposals (signature, occurrences, status, last_text, tool_names, tool_count, updated_at)
             VALUES ('sig1', 2, 'proposed', 'task text', '[\"webfetch\",\"websearch\"]', 4, '2026-01-01')",
            [],
        )
        .unwrap();
        // Signature is the dedupe key: a second row for it is rejected.
        let dup = conn.execute(
            "INSERT INTO skill_proposals (signature, occurrences, status, last_text, tool_names, tool_count, updated_at)
             VALUES ('sig1', 1, 'seen', 'x', '[]', 0, '2026-01-01')",
            [],
        );
        assert!(dup.is_err(), "duplicate signature accepted");
        // CHECK keeps the status domain closed.
        let bad = conn.execute("UPDATE skill_proposals SET status = 'bogus' WHERE signature = 'sig1'", []);
        assert!(bad.is_err(), "unknown status accepted");
    }

    #[test]
    fn migration_v38_adds_archived_at_nullable() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        conn.execute("ALTER TABLE projects DROP COLUMN archived_at", [])
            .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 37).unwrap();

        run_migrations(&conn).unwrap();

        let stored: Option<String> = conn
            .query_row("SELECT archived_at FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
        conn.execute(
            "UPDATE projects SET archived_at = '2026-01-01 10:00:00' WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE projects SET archived_at = NULL WHERE id = 1",
            [],
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 37).unwrap();
        run_migrations(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v39_adds_unread_at_nullable() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        conn.execute("ALTER TABLE schedules DROP COLUMN unread_at", [])
            .unwrap();
        conn.execute(
            "INSERT INTO schedules (kind, spec, status, created_at) VALUES ('cron', '0 10 * * *', 'active', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 38).unwrap();

        run_migrations(&conn).unwrap();

        let stored: Option<String> = conn
            .query_row("SELECT unread_at FROM schedules WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, None);
        conn.execute(
            "UPDATE schedules SET unread_at = '2026-01-01T10:00:00Z' WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE schedules SET unread_at = NULL WHERE id = 1",
            [],
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 38).unwrap();
        run_migrations(&conn).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
