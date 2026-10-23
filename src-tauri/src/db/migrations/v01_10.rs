// Migrations v1-v10: base project/task/chat/memory schema plus the first
// vector and FTS tables. SQL strings and order are unchanged from the
// original schema.rs; only module boundaries moved.
use crate::error::Result;
use rusqlite::Connection;

const MIGRATION_1: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    opened_at TEXT
);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'new' CHECK(status IN ('new','progress','done')),
    comment TEXT,
    due_date TEXT,
    remind_at TEXT,
    tags TEXT NOT NULL DEFAULT '[]',
    position REAL NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    closed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_projects_opened ON projects(opened_at);
CREATE INDEX IF NOT EXISTS idx_tasks_project_status ON tasks(project_id, status);
CREATE INDEX IF NOT EXISTS idx_tasks_due ON tasks(due_date) WHERE due_date IS NOT NULL;
";

const MIGRATION_2: &str = "
CREATE TABLE IF NOT EXISTS indexed_files (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    content_hash TEXT,
    indexed_at TEXT NOT NULL,
    PRIMARY KEY (project_id, rel_path)
);
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    rel_path UNINDEXED,
    project_id UNINDEXED,
    name,
    content,
    tokenize = 'unicode61'
);
";

// vec_items rowids are referenced from file_vectors.vec_rowid: file_vectors is
// the authoritative metadata row per (project, file, kind); vec_items stores
// only the vector. One mapping column instead of a second table.
pub(super) fn migration_3() -> String {
    format!(
        "
CREATE TABLE IF NOT EXISTS file_vectors (
    project_id INTEGER NOT NULL,
    rel_path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('text','name')),
    dim INTEGER NOT NULL,
    backend TEXT NOT NULL,
    text_hash TEXT,
    vec_rowid INTEGER,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_id, rel_path, kind)
);
CREATE INDEX IF NOT EXISTS idx_file_vectors_rowid ON file_vectors(vec_rowid);
CREATE VIRTUAL TABLE IF NOT EXISTS vec_items USING vec0(
    project_id INTEGER PARTITION KEY,
    embedding float[{}]
);
",
        crate::embed::EMBED_DIM
    )
}

// One continuous chat per project (SPEC 3.3, 4.6): the partial unique index
// enforces exactly one 'project' chat per project, while harness sessions
// (kind='harness', project_id NULL) stay independent. messages is the
// append-only transcript; tool calls are stored as a JSON array of
// {id, name, arguments} with arguments kept as the raw model string.
const MIGRATION_4: &str = "
CREATE TABLE IF NOT EXISTS chats (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('project','harness')),
    title TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uniq_chats_one_project_chat ON chats(project_id) WHERE kind = 'project';
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content TEXT,
    tool_calls TEXT,
    tool_call_id TEXT,
    name TEXT,
    created_at TEXT NOT NULL,
    position INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_chat_position ON messages(chat_id, position);
";

// Checkpoint summaries for context compaction (SPEC 4.6, 6.4): each row
// folds transcript positions <= up_to_position into one summary; the raw
// messages stay append-only so the model-facing prefix stays cache-friendly.
const MIGRATION_5: &str = "
CREATE TABLE IF NOT EXISTS summaries (
    id INTEGER PRIMARY KEY,
    chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    up_to_position INTEGER NOT NULL,
    summary TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_summaries_chat ON summaries(chat_id, up_to_position);
";

// Harness sessions (SPEC 5.1, 5.3). A session is its own identity row (title +
// TTL timestamps); its messages reuse the existing chats/messages tables via a
// chats row with kind='harness' linked by session_id — message storage is not
// duplicated. Deleting a session cascades to its chat (→ messages, summaries)
// and to the schedules created in it. The schedules table lands now; the
// scheduler engine is a later slice.
const MIGRATION_6: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    title TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_active_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS schedules (
    id INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('run_at','cron','notify')),
    spec TEXT NOT NULL,
    prompt TEXT,
    status TEXT NOT NULL CHECK(status IN ('active','paused','done','error')),
    related_task_id INTEGER,
    created_at TEXT NOT NULL,
    last_run_at TEXT,
    next_run_at TEXT
);
ALTER TABLE chats ADD COLUMN session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE;
";

// Global memory (SPEC 6.3): one store shared by project chats and harness
// sessions. Deleting a project keeps its memory records but detaches them
// (SET NULL). memory_fts mirrors memory.content with rowid = memory.id;
// FTS5 keyword search is enough at M1-scale memory, no vector leg yet.
const MIGRATION_7: &str = "
CREATE TABLE IF NOT EXISTS memory (
    id INTEGER PRIMARY KEY,
    content TEXT NOT NULL,
    project_id INTEGER REFERENCES projects(id) ON DELETE SET NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    source TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    content,
    tokenize = 'unicode61'
);
";

// Scheduler engine (SPEC 5.3): the IANA timezone a schedule fires in (NULL =
// system zone) and the message of the last failed run. run_at/cron/next_run_at
// timestamps are stored as RFC3339 UTC so string comparison orders them.
const MIGRATION_8: &str = "
ALTER TABLE schedules ADD COLUMN timezone TEXT;
ALTER TABLE schedules ADD COLUMN error TEXT;
";

// Task search (SPEC 4.4): tasks_fts mirrors title/comment/tags keyed by task
// id, synced from tasks.rs on every mutation; search joins tasks for project
// scoping and status/dates, so closed tasks stay findable. contentless keeps
// the mirror small; contentless_delete=1 allows the delete+reinsert that
// stands in for UPDATE (plain contentless tables reject DELETE).
const MIGRATION_9: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS tasks_fts USING fts5(
    title,
    comment,
    tags,
    content = '',
    contentless_delete = 1,
    tokenize = 'unicode61'
);
";

// Chat attachments (SPEC 6.1): one row per attachment copy saved by
// attach_files and linked to the user message that carried it. saved_path is
// project-relative for project chats and absolute for harness sessions. The
// copied file itself is a regular project/harness file and stays on disk;
// deleting the message cascades only the link row.
const MIGRATION_10: &str = "
CREATE TABLE IF NOT EXISTS message_attachments (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    saved_path TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_image INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_message_attachments_message ON message_attachments(message_id);
";

pub(super) fn migrate_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_1)?;
    Ok(())
}

pub(super) fn migrate_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_2)?;
    Ok(())
}

pub(super) fn migrate_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(&migration_3())?;
    Ok(())
}

pub(super) fn migrate_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_4)?;
    Ok(())
}

pub(super) fn migrate_v5(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_5)?;
    Ok(())
}

pub(super) fn migrate_v6(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_6)?;
    Ok(())
}

pub(super) fn migrate_v7(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_7)?;
    Ok(())
}

pub(super) fn migrate_v8(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_8)?;
    Ok(())
}

pub(super) fn migrate_v9(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_9)?;
    Ok(())
}

pub(super) fn migrate_v10(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_10)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::schema::SCHEMA_VERSION;
    use rusqlite::params;

    #[test]
    fn migration_v3_creates_vector_tables() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('file_vectors','vec_items')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
        let bad = conn.execute(
            "INSERT INTO file_vectors (project_id, rel_path, kind, dim, backend, updated_at) VALUES (1, 'a', 'bogus', 256, 'noop', '2026-01-01')",
            [],
        );
        assert!(bad.is_err());
    }

    #[test]
    fn migration_v4_enforces_one_project_chat_but_allows_harness_sessions() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let second = conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(second.is_err(), "second project chat for the same project accepted");

        // harness sessions are not bound to a project and stay independent
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO chats (kind, created_at, updated_at) VALUES ('harness', '2026-01-01', '2026-01-01')",
                [],
            )
            .unwrap();
        }
        let bad_kind = conn.execute(
            "INSERT INTO chats (kind, created_at, updated_at) VALUES ('bogus', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(bad_kind.is_err());

        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (1, 'user', 'hi', '2026-01-01', 1)",
            [],
        )
        .unwrap();
        let orphan = conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (99, 'user', 'x', '2026-01-01', 1)",
            [],
        );
        assert!(orphan.is_err(), "message without a chat accepted");
    }

    #[test]
    fn migration_v5_stores_checkpoint_summaries_with_fk() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO summaries (chat_id, up_to_position, summary, created_at) VALUES (1, 10, 'folded', '2026-01-01')",
            [],
        )
        .unwrap();
        let orphan = conn.execute(
            "INSERT INTO summaries (chat_id, up_to_position, summary, created_at) VALUES (99, 1, 'x', '2026-01-01')",
            [],
        );
        assert!(orphan.is_err(), "summary without a chat accepted");
    }

    #[test]
    fn migration_v6_session_deletion_cascades_to_chat_but_v12_detaches_schedules() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();

        conn.execute(
            "INSERT INTO sessions (title, created_at, updated_at, last_active_at) VALUES ('s', '2026-01-01', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chats (kind, session_id, created_at, updated_at) VALUES ('harness', ?1, '2026-01-01', '2026-01-01')",
            params![session_id],
        )
        .unwrap();
        let chat_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (?1, 'user', 'hi', '2026-01-01', 1)",
            params![chat_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, status, created_at) VALUES (?1, 'cron', '0 10 * * *', 'active', '2026-01-01')",
            params![session_id],
        )
        .unwrap();
        let schedule_id = conn.last_insert_rowid();

        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .unwrap();
        let chats: i64 = conn
            .query_row("SELECT count(*) FROM chats WHERE id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        let messages: i64 = conn
            .query_row("SELECT count(*) FROM messages WHERE chat_id = ?1", params![chat_id], |r| r.get(0))
            .unwrap();
        // Since v12 the schedule row survives with session_id detached to NULL.
        let schedule_session: Option<i64> = conn
            .query_row("SELECT session_id FROM schedules WHERE id = ?1", params![schedule_id], |r| r.get(0))
            .unwrap();
        assert_eq!((chats, messages), (0, 0));
        assert_eq!(schedule_session, None);

        // CHECK constraints reject unknown schedule kind/status.
        conn.execute(
            "INSERT INTO sessions (created_at, updated_at, last_active_at) VALUES ('2026-01-01','2026-01-01','2026-01-01')",
            [],
        )
        .unwrap();
        let new_session = conn.last_insert_rowid();
        let bad_kind = conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, status, created_at) VALUES (?1, 'bogus', 'x', 'active', '2026-01-01')",
            params![new_session],
        );
        assert!(bad_kind.is_err());
        let bad_status = conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, status, created_at) VALUES (?1, 'run_at', 'x', 'bogus', '2026-01-01')",
            params![new_session],
        );
        assert!(bad_status.is_err());
    }

    #[test]
    fn migration_v7_creates_memory_and_memory_fts() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('memory','memory_fts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v8_adds_schedule_timezone_and_error_columns() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('schedules')").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for name in ["timezone", "error"] {
            assert!(cols.iter().any(|c| c == name), "missing column {name}");
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v9_creates_tasks_fts() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'tasks_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v10_links_message_attachments_and_cascades_from_message() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (1, 'user', 'hi', '2026-01-01', 1)",
            [],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO message_attachments (message_id, name, saved_path, kind, is_image, created_at) VALUES (?1, 'a.txt', 'Входящие/a.txt', 'document', 0, '2026-01-01')",
            params![message_id],
        )
        .unwrap();

        let orphan = conn.execute(
            "INSERT INTO message_attachments (message_id, name, saved_path, kind, is_image, created_at) VALUES (99, 'x', 'x', 'other', 0, '2026-01-01')",
            [],
        );
        assert!(orphan.is_err(), "attachment without a message accepted");

        // No message-delete command exists yet, so the cascade is asserted
        // straight against the FK: removing the message removes its links.
        conn.execute("DELETE FROM messages WHERE id = ?1", params![message_id])
            .unwrap();
        let attachments: i64 = conn
            .query_row(
                "SELECT count(*) FROM message_attachments WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attachments, 0);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
