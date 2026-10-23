// Migrations v21-v30: background processes, thinking traces, schedule title,
// trash/versions, task upgrades, undo log, objectives, and entity links.
// SQL strings and order are unchanged from the original schema.rs.
use crate::error::Result;
use rusqlite::{params, Connection};

// Background process control (harness process tools): one row per started
// background command (dev servers, installs). The command runs in its own
// process group, so pgid equals pid; logs live under Harness/tmp/processes/
// and are plain files, not FK-linked — deleting a row never touches a log.
// status: running → exited (pid gone) / stopped (killed via process_stop)
// / error (exited non-zero or otherwise unusable).
const MIGRATION_21: &str = "
CREATE TABLE IF NOT EXISTS processes (
    id INTEGER PRIMARY KEY,
    name TEXT,
    pid INTEGER NOT NULL,
    pgid INTEGER NOT NULL,
    command TEXT NOT NULL,
    cwd TEXT,
    log_path TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('running','exited','stopped','error')),
    exit_code INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

// Thinking-mode traces: the assistant chain of thought is stored next to its
// turn so a follow-up request can replay `reasoning_content` (thinking-mode
// providers reject a transcript that drops it) and the UI can show it
// collapsed. Guarded per column (same recipe as v14) so a re-run after an
// interrupted v22 does not re-ALTER.
const MIGRATION_22: &str = "
ALTER TABLE messages ADD COLUMN reasoning TEXT;
";

// Schedules UI (SPEC 5.3): a short human title per schedule, shown in the
// panel instead of the raw spec. Guarded per column (v14/v22 recipe); the
// backfill runs in Rust because "first non-empty line of the prompt, cut to
// 80 chars" is scheduler logic (derive_title), falling back to the spec.
const MIGRATION_23: &str = "
ALTER TABLE schedules ADD COLUMN title TEXT;
";

// Trash and version history (SPEC trash/versions slice): every delete moves
// the item into Settings/.trash/<id>/<name> and keeps a row here; every
// overwrite of an existing file snapshots the previous content into
// Settings/.versions/<id>. kind 'task' rows carry the whole task as JSON in
// meta with an empty stored_path (no file on disk). No indexes: both tables
// stay small under the 30-day retention and the per-path version cap.
const MIGRATION_24: &str = "
CREATE TABLE IF NOT EXISTS trash (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    kind TEXT NOT NULL,
    original_path TEXT NOT NULL,
    stored_path TEXT NOT NULL,
    name TEXT NOT NULL,
    deleted_at TEXT NOT NULL,
    meta TEXT
);
CREATE TABLE IF NOT EXISTS versions (
    id INTEGER PRIMARY KEY,
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    stored_path TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL
);
";

// Task upgrades (SPEC task slice): priority, checklist (JSON array of
// {text, done}) and recurrence. ALTER ADD COLUMN accepts CHECK constraints,
// so priority/recurrence are guarded in the schema too; tasks.rs validates in
// Rust for readable errors. NOT NULL DEFAULT backfills every existing row.
const MIGRATION_25_PRIORITY: &str =
    "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal' CHECK(priority IN ('low','normal','high','urgent'))";
const MIGRATION_25_CHECKLIST: &str = "ALTER TABLE tasks ADD COLUMN checklist TEXT NOT NULL DEFAULT '[]'";
const MIGRATION_25_RECURRENCE: &str =
    "ALTER TABLE tasks ADD COLUMN recurrence TEXT NOT NULL DEFAULT 'none' CHECK(recurrence IN ('none','daily','weekly','monthly'))";

// Message-level chat UX (SPEC chat slice): a pinned message shows in the
// compact bar above the transcript. Guarded per column (v14/v22 recipe) so a
// re-run after an interrupted v26 does not re-ALTER.
const MIGRATION_26_PINNED: &str = "ALTER TABLE messages ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0";

// Scheduler storm guard (SPEC 5.3): how many times a failed run was retried;
// the engine retries once (retry_count < 1) and then flips to error. Guarded
// per column (v14/v22 recipe) so a re-run after an interrupted v27 does not
// re-ALTER.
const MIGRATION_27_RETRY_COUNT: &str =
    "ALTER TABLE schedules ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0";

// Bounded undo stack (SPEC undo slice): one row per reversible mutation. The
// payload is a JSON blob whose shape depends on kind; the stack is pruned to
// the newest UNDO_LIMIT rows after every push.
const MIGRATION_28_UNDO_LOG: &str = "
CREATE TABLE IF NOT EXISTS undo_log (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);
";

// Long-lived goals with steps (objectives slice): objectives are global, not
// bound to a project; steps are an ordered checklist that cascades with the
// objective. No indexes: both tables stay small (Today panel + agent tools).
const MIGRATION_29_OBJECTIVES: &str = "
CREATE TABLE IF NOT EXISTS objectives (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','paused','done','error')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS objective_steps (
    id INTEGER PRIMARY KEY,
    objective_id INTEGER NOT NULL REFERENCES objectives(id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    position INTEGER NOT NULL
);
";

// Entity links (links slice): undirected relationships between entities,
// recorded automatically after successful agent tool calls and context
// actions. Kinds: project, file, task, note, chat, schedule, objective,
// message. Ids are strings: files are "<projectId>:<relPath>", notes are the
// vault-relative path, everything else is the numeric row id as text. The
// UNIQUE constraint is the dedupe key (INSERT OR IGNORE); it also backs the
// from-side lookups, so no extra index is needed at this table's size.
const MIGRATION_30_LINKS: &str = "
CREATE TABLE IF NOT EXISTS links (
    id INTEGER PRIMARY KEY,
    from_kind TEXT NOT NULL,
    from_id TEXT NOT NULL,
    to_kind TEXT NOT NULL,
    to_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(from_kind, from_id, to_kind, to_id)
);
";

pub(super) fn migrate_v21(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_21)?;
    Ok(())
}

pub(super) fn migrate_v22(conn: &Connection) -> Result<()> {
    let has_reasoning: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'reasoning'",
        [],
        |r| r.get(0),
    )?;
    if has_reasoning == 0 {
        conn.execute_batch(MIGRATION_22)?;
    }
    Ok(())
}

pub(super) fn migrate_v23(conn: &Connection) -> Result<()> {
    let has_title: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('schedules') WHERE name = 'title'",
        [],
        |r| r.get(0),
    )?;
    if has_title == 0 {
        conn.execute_batch(MIGRATION_23)?;
    }
    let rows: Vec<(i64, Option<String>, String)> = {
        let mut stmt = conn.prepare("SELECT id, prompt, spec FROM schedules WHERE title IS NULL")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, prompt, spec) in rows {
        let title = crate::scheduler::derive_title(prompt.as_deref(), &spec, 80);
        conn.execute(
            "UPDATE schedules SET title = ?1 WHERE id = ?2",
            params![title, id],
        )?;
    }
    Ok(())
}

pub(super) fn migrate_v24(conn: &Connection) -> Result<()> {
    // Guarded per table (v14 recipe) so a re-run after an interrupted
    // v24 does not re-create; CREATE IF NOT EXISTS is idempotent anyway.
    let has_trash: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = 'trash'",
        [],
        |r| r.get(0),
    )?;
    if has_trash == 0 {
        conn.execute_batch(MIGRATION_24)?;
    }
    Ok(())
}

pub(super) fn migrate_v25(conn: &Connection) -> Result<()> {
    // Guarded per column (v14 recipe) so a re-run after an interrupted
    // v25 does not re-ALTER.
    for (column, ddl) in [
        ("priority", MIGRATION_25_PRIORITY),
        ("checklist", MIGRATION_25_CHECKLIST),
        ("recurrence", MIGRATION_25_RECURRENCE),
    ] {
        let has: i64 = conn.query_row(
            "SELECT count(*) FROM pragma_table_info('tasks') WHERE name = ?1",
            params![column],
            |r| r.get(0),
        )?;
        if has == 0 {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

pub(super) fn migrate_v26(conn: &Connection) -> Result<()> {
    let has_pinned: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'pinned'",
        [],
        |r| r.get(0),
    )?;
    if has_pinned == 0 {
        conn.execute(MIGRATION_26_PINNED, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v27(conn: &Connection) -> Result<()> {
    let has_retry_count: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('schedules') WHERE name = 'retry_count'",
        [],
        |r| r.get(0),
    )?;
    if has_retry_count == 0 {
        conn.execute(MIGRATION_27_RETRY_COUNT, [])?;
    }
    Ok(())
}

pub(super) fn migrate_v28(conn: &Connection) -> Result<()> {
    let has_undo_log: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = 'undo_log'",
        [],
        |r| r.get(0),
    )?;
    if has_undo_log == 0 {
        conn.execute_batch(MIGRATION_28_UNDO_LOG)?;
    }
    Ok(())
}

pub(super) fn migrate_v29(conn: &Connection) -> Result<()> {
    // Guarded per table (v24/v28 recipe) so a re-run after an interrupted
    // v29 does not re-create; CREATE IF NOT EXISTS is idempotent anyway.
    let has_objectives: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = 'objectives'",
        [],
        |r| r.get(0),
    )?;
    if has_objectives == 0 {
        conn.execute_batch(MIGRATION_29_OBJECTIVES)?;
    }
    Ok(())
}

pub(super) fn migrate_v30(conn: &Connection) -> Result<()> {
    // Guarded per table (v24/v28 recipe) so a re-run after an interrupted
    // v30 does not re-create; CREATE IF NOT EXISTS is idempotent anyway.
    let has_links: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = 'links'",
        [],
        |r| r.get(0),
    )?;
    if has_links == 0 {
        conn.execute_batch(MIGRATION_30_LINKS)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::schema::SCHEMA_VERSION;

    #[test]
    fn migration_v21_creates_processes_table_with_status_check() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO processes (name, pid, pgid, command, cwd, log_path, status, exit_code, created_at, updated_at) VALUES ('dev', 4242, 4242, 'npm run dev', '/tmp/p', '/tmp/p.log', 'exited', 0, '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        // name and cwd stay nullable, exit_code too
        conn.execute(
            "INSERT INTO processes (pid, pgid, command, log_path, status, created_at, updated_at) VALUES (1, 1, 'sleep 1', 'l', 'running', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        for bad_status in ["bogus", "killed", ""] {
            let bad = conn.execute(
                "INSERT INTO processes (pid, pgid, command, log_path, status, created_at, updated_at) VALUES (2, 2, 'x', 'l', ?1, '2026-01-01', '2026-01-01')",
                params![bad_status],
            );
            assert!(bad.is_err(), "status {bad_status:?} accepted");
        }
        // pid/pgid/command/log_path/status are NOT NULL
        let no_pid = conn.execute(
            "INSERT INTO processes (pgid, command, log_path, status, created_at, updated_at) VALUES (1, 'x', 'l', 'running', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(no_pid.is_err());

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v22_adds_nullable_messages_reasoning_column() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let has_reasoning: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'reasoning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_reasoning, 1);

        conn.execute(
            "INSERT INTO chats (id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        // Nullable: a turn without a thinking trace stores NULL.
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (1, 'assistant', 'answer', '2026-01-01', 1)",
            [],
        )
        .unwrap();
        let missing: Option<String> = conn
            .query_row("SELECT reasoning FROM messages WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, None);
        conn.execute(
            "UPDATE messages SET reasoning = ?1 WHERE id = 1",
            params!["because"],
        )
        .unwrap();
        let stored: String = conn
            .query_row("SELECT reasoning FROM messages WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "because");

        // Guarded ALTER: a v21 database whose column is missing is repaired.
        conn.execute("ALTER TABLE messages DROP COLUMN reasoning", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 21).unwrap();
        run_migrations(&conn).unwrap();
        let has_reasoning: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'reasoning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_reasoning, 1);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v23_adds_schedule_title_and_backfills_from_prompt_or_spec() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v23 database: no title column, rows without titles.
        conn.execute("ALTER TABLE schedules DROP COLUMN title", [])
            .unwrap();
        // Multi-line prompt: the first non-empty line, trimmed.
        conn.execute(
            "INSERT INTO schedules (kind, spec, prompt, status, created_at) VALUES ('cron', '0 10 * * *', ?1, 'active', '2026-01-01')",
            params!["\n\n   Проверить конкурентов и подготовить отчёт по ценам \nвторая строка"],
        )
        .unwrap();
        // A 100-char first line is cut to 80.
        conn.execute(
            "INSERT INTO schedules (kind, spec, prompt, status, created_at) VALUES ('run_at', '2026-01-02T10:00:00Z', ?1, 'active', '2026-01-01')",
            params!["к".repeat(100)],
        )
        .unwrap();
        // No prompt: the spec becomes the title.
        conn.execute(
            "INSERT INTO schedules (kind, spec, prompt, status, created_at) VALUES ('notify', '2026-01-03T10:00:00Z', NULL, 'active', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 22).unwrap();

        run_migrations(&conn).unwrap();

        let titles: Vec<Option<String>> = {
            let mut stmt = conn.prepare("SELECT title FROM schedules ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            titles[0].as_deref(),
            Some("Проверить конкурентов и подготовить отчёт по ценам")
        );
        assert_eq!(titles[1].as_deref().unwrap().chars().count(), 80);
        assert_eq!(titles[2].as_deref(), Some("2026-01-03T10:00:00Z"));

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v24_creates_trash_and_versions_tables() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('trash','versions')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
        conn.execute(
            "INSERT INTO trash (scope, kind, original_path, stored_path, name, deleted_at) VALUES ('project', 'file', '/p/a.txt', '/w/Settings/.trash/u/a.txt', 'a.txt', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO versions (scope, path, stored_path, bytes, created_at) VALUES ('project', '/p/a.txt', '/w/Settings/.versions/u', 3, '2026-01-01')",
            [],
        )
        .unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v25_adds_task_priority_checklist_recurrence_with_defaults_and_checks() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, created_at) VALUES (1, 't', '2026-01-01')",
            [],
        )
        .unwrap();
        let (priority, checklist, recurrence): (String, String, String) = conn
            .query_row(
                "SELECT priority, checklist, recurrence FROM tasks WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((priority.as_str(), checklist.as_str(), recurrence.as_str()), ("normal", "[]", "none"));

        for bad in ["bogus", "critical", ""] {
            let err = conn.execute(
                "UPDATE tasks SET priority = ?1 WHERE id = 1",
                params![bad],
            );
            assert!(err.is_err(), "priority {bad:?} accepted");
        }
        for bad in ["yearly", ""] {
            let err = conn.execute("UPDATE tasks SET recurrence = ?1 WHERE id = 1", params![bad]);
            assert!(err.is_err(), "recurrence {bad:?} accepted");
        }
        conn.execute("UPDATE tasks SET priority = 'urgent', recurrence = 'monthly' WHERE id = 1", [])
            .unwrap();

        // Guarded ALTER: a v24 database missing the columns is repaired.
        conn.execute("ALTER TABLE tasks DROP COLUMN checklist", []).unwrap();
        conn.pragma_update(None, "user_version", 24).unwrap();
        run_migrations(&conn).unwrap();
        let has_checklist: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('tasks') WHERE name = 'checklist'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_checklist, 1);
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v26_adds_message_pinned_default_zero() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let has_pinned: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'pinned'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_pinned, 1);

        conn.execute(
            "INSERT INTO chats (id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (1, 'user', 'hi', '2026-01-01', 1)",
            [],
        )
        .unwrap();
        // NOT NULL DEFAULT backfills every existing row with 0.
        let pinned: i64 = conn
            .query_row("SELECT pinned FROM messages WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pinned, 0);

        // Guarded ALTER: a v25 database missing the column is repaired.
        conn.execute("ALTER TABLE messages DROP COLUMN pinned", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 25).unwrap();
        run_migrations(&conn).unwrap();
        let has_pinned: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'pinned'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_pinned, 1);
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v27_adds_schedule_retry_count_default_zero() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let has_retry_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('schedules') WHERE name = 'retry_count'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_retry_count, 1);

        conn.execute(
            "INSERT INTO schedules (kind, spec, status, created_at) VALUES ('cron', '0 10 * * *', 'active', '2026-01-01')",
            [],
        )
        .unwrap();
        // NOT NULL DEFAULT backfills every existing row with 0.
        let retry_count: i64 = conn
            .query_row("SELECT retry_count FROM schedules WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(retry_count, 0);

        // Guarded ALTER: a v26 database missing the column is repaired.
        conn.execute("ALTER TABLE schedules DROP COLUMN retry_count", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 26).unwrap();
        run_migrations(&conn).unwrap();
        let has_retry_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('schedules') WHERE name = 'retry_count'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_retry_count, 1);
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v29_creates_objectives_with_status_check_and_step_cascade() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();

        conn.execute(
            "INSERT INTO objectives (title, created_at, updated_at) VALUES ('g', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let objective_id = conn.last_insert_rowid();
        // status defaults to 'active', description stays nullable.
        let (status, description): (String, Option<String>) = conn
            .query_row(
                "SELECT status, description FROM objectives WHERE id = ?1",
                params![objective_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((status.as_str(), description.as_deref()), ("active", None));

        conn.execute(
            "INSERT INTO objective_steps (objective_id, text, position) VALUES (?1, 'шаг 1', 1)",
            params![objective_id],
        )
        .unwrap();
        // done defaults to 0.
        let done: i64 = conn
            .query_row("SELECT done FROM objective_steps WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(done, 0);

        for bad_status in ["bogus", "new", ""] {
            let bad = conn.execute(
                "UPDATE objectives SET status = ?1 WHERE id = ?2",
                params![bad_status, objective_id],
            );
            assert!(bad.is_err(), "status {bad_status:?} accepted");
        }
        for status in ["active", "paused", "done", "error"] {
            conn.execute(
                "UPDATE objectives SET status = ?1 WHERE id = ?2",
                params![status, objective_id],
            )
            .unwrap();
        }

        // A step without an objective is rejected; deleting the objective
        // cascades to its steps.
        let orphan = conn.execute(
            "INSERT INTO objective_steps (objective_id, text, position) VALUES (99, 'x', 1)",
            [],
        );
        assert!(orphan.is_err(), "step without an objective accepted");
        conn.execute("DELETE FROM objectives WHERE id = ?1", params![objective_id])
            .unwrap();
        let steps: i64 = conn
            .query_row(
                "SELECT count(*) FROM objective_steps WHERE objective_id = ?1",
                params![objective_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(steps, 0, "steps survived the objective delete");

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
