// Migrations v11-v20: vector-dim re-creations, table rebuilds (schedules,
// indexed_files, chats), project brief, and chat/memory hybrid search tables.
// SQL strings and order are unchanged from the original schema.rs.
use crate::error::Result;
use rusqlite::Connection;

// Embedding vector dim 256 -> 512 (EmbeddingGemma 2 MRL truncation): a vec0
// table cannot be resized in place, so drop and recreate it with the current
// EMBED_DIM. Every stored vector is invalid afterwards, so file_vectors is
// cleared too: a full reindex/backfill (reindex_all) is required after this
// migration; FTS rows in indexed_files/files_fts are untouched.
fn migration_11() -> String {
    format!(
        "
DROP TABLE IF EXISTS vec_items;
DROP TABLE IF EXISTS file_vectors;
{}
",
        super::v01_10::migration_3()
    )
}

// Schedules survive session deletion (SPEC 5.3 decision): a schedule
// remembers its session, and when that session is gone the schedule fires
// into a fresh one. SQLite cannot change a column constraint in place, so
// the table is rebuilt: create the new shape (identical columns/CHECKs, FK
// now ON DELETE SET NULL), copy every row, drop the old table, rename.
// No indexes exist on schedules; the rename carries any along.
const MIGRATION_12: &str = "
CREATE TABLE schedules_new (
    id INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK(kind IN ('run_at','cron','notify')),
    spec TEXT NOT NULL,
    prompt TEXT,
    status TEXT NOT NULL CHECK(status IN ('active','paused','done','error')),
    related_task_id INTEGER,
    created_at TEXT NOT NULL,
    last_run_at TEXT,
    next_run_at TEXT,
    timezone TEXT,
    error TEXT
);
INSERT INTO schedules_new (id, session_id, kind, spec, prompt, status, related_task_id, created_at, last_run_at, next_run_at, timezone, error)
    SELECT id, session_id, kind, spec, prompt, status, related_task_id, created_at, last_run_at, next_run_at, timezone, error FROM schedules;
DROP TABLE schedules;
ALTER TABLE schedules_new RENAME TO schedules;
";

// Embedding vector dim 512 -> 768 (full EmbeddingGemma 2 quality, no MRL
// truncation): same drop/recreate as v11. Every stored 512-d vector is
// invalid afterwards, so file_vectors is cleared too: a full
// reindex/backfill (reindex_all) is required after this migration; FTS rows
// in indexed_files/files_fts are untouched.
fn migration_13() -> String {
    format!(
        "
DROP TABLE IF EXISTS vec_items;
DROP TABLE IF EXISTS file_vectors;
{}
",
        super::v01_10::migration_3()
    )
}

// Manual project ordering: projects are listed by position, not opened_at, so
// opening a project no longer reorders the list. Backfill position = id so
// existing rows keep a stable creation-order sequence.
const MIGRATION_14: &str = "
ALTER TABLE projects ADD COLUMN position REAL NOT NULL DEFAULT 0;
UPDATE projects SET position = id;
";

// Per-conversation TODO plans (todowrite/todoread tools): one ordered list
// per scope — a project chat ('project', scope_id = project id) or a harness
// session ('session', scope_id = session id). scope_kind/scope_id are not
// FKs: project and session ids come from different tables and may collide
// numerically, the kind column separates them (same reasoning as RunKey).
// The tool replaces the whole list per write, so no cascade is wired: rows
// left by a deleted scope are unreachable and harmless.
const MIGRATION_15: &str = "
CREATE TABLE IF NOT EXISTS todos (
    id INTEGER PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK(scope_kind IN ('project','session')),
    scope_id INTEGER NOT NULL,
    text TEXT NOT NULL,
    done INTEGER NOT NULL DEFAULT 0,
    position INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_todos_scope ON todos(scope_kind, scope_id, position);
";

// Notes vault (SPEC notes slice): notes are indexed under the reserved
// scope id NOTES_SCOPE_ID = -1, which has no projects row, so the
// indexed_files FK to projects(id) must go. SQLite cannot drop a column
// constraint in place: rebuild the table (identical columns/PK, no FK),
// copy every row, drop, rename. The lost ON DELETE CASCADE is unused (no
// project-delete command exists); the indexer walk prunes stale rows.
// files_fts/file_vectors/vec_items never had that FK, so they stay as is.
const MIGRATION_16: &str = "
CREATE TABLE indexed_files_new (
    project_id INTEGER NOT NULL,
    rel_path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    content_hash TEXT,
    indexed_at TEXT NOT NULL,
    PRIMARY KEY (project_id, rel_path)
);
INSERT INTO indexed_files_new (project_id, rel_path, name, kind, size, mtime, content_hash, indexed_at)
    SELECT project_id, rel_path, name, kind, size, mtime, content_hash, indexed_at FROM indexed_files;
DROP TABLE indexed_files;
ALTER TABLE indexed_files_new RENAME TO indexed_files;
";

// Notes chat (SPEC notes chat slice): chats.kind gains 'notes' — one
// continuous notes chat per working dir. SQLite cannot widen a CHECK in
// place, so the table is rebuilt (identical columns/FKs, widened CHECK),
// every row is copied, the old table is dropped, the rename is followed by
// recreating both partial unique indexes (DROP TABLE took them with it).
// chats is a PARENT of messages/summaries: with FK enforcement on, that
// DROP would cascade-delete every transcript, so run_migrations turns
// foreign_keys off around this rebuild and restores the previous setting.
const MIGRATION_17: &str = "
CREATE TABLE chats_new (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('project','harness','notes')),
    title TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE
);
INSERT INTO chats_new (id, project_id, kind, title, created_at, updated_at, session_id)
    SELECT id, project_id, kind, title, created_at, updated_at, session_id FROM chats;
DROP TABLE chats;
ALTER TABLE chats_new RENAME TO chats;
CREATE UNIQUE INDEX IF NOT EXISTS uniq_chats_one_project_chat ON chats(project_id) WHERE kind = 'project';
CREATE UNIQUE INDEX IF NOT EXISTS uniq_chats_one_notes_chat ON chats(kind) WHERE kind = 'notes';
";

// Harness chat search: messages_fts mirrors user/assistant turns of
// kind='harness' chats (message id as a plain column, session id for direct
// session mapping), message_vectors tracks which messages already carry an
// embedding, vec_messages stores one vector per message under rowid =
// message_id (no partition key: harness transcripts stay small). Existing
// transcripts are backfilled into messages_fts here; vectors are backfilled
// on demand by the index_chat_messages command because the embedding model
// may be missing at migration time.
fn migration_19() -> String {
    format!(
        "
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    message_id UNINDEXED,
    session_id UNINDEXED,
    content,
    tokenize = 'unicode61'
);
CREATE TABLE IF NOT EXISTS message_vectors (
    message_id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS vec_messages USING vec0(
    embedding float[{}]
);
INSERT INTO messages_fts (message_id, session_id, content)
SELECT m.id, c.session_id, m.content
FROM messages m
JOIN chats c ON c.id = m.chat_id AND c.kind = 'harness'
WHERE c.session_id IS NOT NULL
  AND m.role IN ('user','assistant')
  AND m.content IS NOT NULL AND trim(m.content) <> '';
",
        crate::embed::EMBED_DIM
    )
}

// Hybrid memory search (SPEC 6.3): memory_fts is rebuilt with an explicit
// memory_id column and an indexed tags column (the v7 mirror only stored
// content), keeping rowid = memory.id; existing rows are backfilled here.
// memory_vectors tracks which memory rows already carry an embedding and
// vec_memory stores one vector per record under rowid = memory.id. Vectors
// are backfilled on demand by the index_memory command because the embedding
// model may be missing at migration time.
fn migration_20() -> String {
    format!(
        "
DROP TABLE IF EXISTS memory_fts;
CREATE VIRTUAL TABLE memory_fts USING fts5(
    memory_id UNINDEXED,
    content,
    tags,
    tokenize = 'unicode61'
);
CREATE TABLE IF NOT EXISTS memory_vectors (
    memory_id INTEGER PRIMARY KEY,
    updated_at TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS vec_memory USING vec0(
    embedding float[{}]
);
INSERT INTO memory_fts (rowid, memory_id, content, tags)
SELECT id, id, content, tags FROM memory;
",
        crate::embed::EMBED_DIM
    )
}

pub(super) fn migrate_v11(conn: &Connection) -> Result<()> {
    conn.execute_batch(&migration_11())?;
    Ok(())
}

pub(super) fn migrate_v12(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_12)?;
    Ok(())
}

pub(super) fn migrate_v13(conn: &Connection) -> Result<()> {
    conn.execute_batch(&migration_13())?;
    Ok(())
}

pub(super) fn migrate_v14(conn: &Connection) -> Result<()> {
    // Guarded so a re-run after an interrupted v14 (or a test that rolls
    // user_version back on an already-migrated db) does not re-ALTER.
    let has_position: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'position'",
        [],
        |r| r.get(0),
    )?;
    if has_position == 0 {
        conn.execute_batch(MIGRATION_14)?;
    }
    Ok(())
}

pub(super) fn migrate_v15(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_15)?;
    Ok(())
}

pub(super) fn migrate_v16(conn: &Connection) -> Result<()> {
    conn.execute_batch(MIGRATION_16)?;
    Ok(())
}

pub(super) fn migrate_v17(conn: &Connection) -> Result<()> {
    // db::open enables foreign_keys before migrations run, and chats is
    // the parent of messages/summaries: the rebuild's DROP TABLE would
    // cascade-delete every transcript. Standard SQLite recipe: FK
    // enforcement off for the rebuild, previous setting restored after.
    let fk_on: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    conn.execute_batch(MIGRATION_17)?;
    conn.pragma_update(None, "foreign_keys", if fk_on { "ON" } else { "OFF" })?;
    Ok(())
}

pub(super) fn migrate_v18(conn: &Connection) -> Result<()> {
    // Cached project brief (SPEC project context slice): a short Russian
    // summary generated from the project's files/tasks and injected into
    // the project system prompt; brief_updated_at drives staleness
    // (auto-refresh on project open / file changes when older than ~24 h).
    // Guarded per column (same recipe as v14) so a re-run after an
    // interrupted v18 does not re-ALTER.
    let has_brief: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'brief'",
        [],
        |r| r.get(0),
    )?;
    if has_brief == 0 {
        conn.execute("ALTER TABLE projects ADD COLUMN brief TEXT", [])?;
    }
    let has_brief_updated_at: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('projects') WHERE name = 'brief_updated_at'",
        [],
        |r| r.get(0),
    )?;
    if has_brief_updated_at == 0 {
        conn.execute("ALTER TABLE projects ADD COLUMN brief_updated_at TEXT", [])?;
    }
    Ok(())
}

pub(super) fn migrate_v19(conn: &Connection) -> Result<()> {
    conn.execute_batch(&migration_19())?;
    Ok(())
}

pub(super) fn migrate_v20(conn: &Connection) -> Result<()> {
    conn.execute_batch(&migration_20())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::schema::SCHEMA_VERSION;
    use rusqlite::params;

    #[test]
    fn migration_v11_recreates_vec_items_at_current_dims_and_clears_vectors() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v11 database: vec0 at float[256] with stale rows.
        conn.execute_batch("DROP TABLE vec_items; DROP TABLE file_vectors;")
            .unwrap();
        conn.execute_batch(
            "
CREATE TABLE file_vectors (
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
CREATE INDEX idx_file_vectors_rowid ON file_vectors(vec_rowid);
CREATE VIRTUAL TABLE vec_items USING vec0(
    project_id INTEGER PARTITION KEY,
    embedding float[256]
);
",
        )
        .unwrap();
        let blob256: Vec<u8> = vec![0.0f32; 256].iter().flat_map(|x| x.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![blob256],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_vectors (project_id, rel_path, kind, dim, backend, vec_rowid, updated_at) VALUES (1, 'a', 'text', 256, 'ort', 1, '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 10).unwrap();

        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let stale: i64 = conn
            .query_row("SELECT count(*) FROM file_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stale, 0, "stale 256-d vector rows survived v11");
        let blob_current: Vec<u8> = vec![0.0f32; crate::embed::EMBED_DIM]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![blob_current],
        )
        .unwrap();
        let wrong_dim = conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![blob256],
        );
        assert!(wrong_dim.is_err(), "256-d vector accepted into the current-dim vec0 table");
    }

    #[test]
    fn migration_v12_rebuilds_schedules_with_set_null_and_keeps_rows() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();

        // Simulate a pre-v12 database: schedules with the old CASCADE FK.
        conn.execute_batch("DROP TABLE schedules;").unwrap();
        conn.execute_batch(
            "
CREATE TABLE schedules (
    id INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('run_at','cron','notify')),
    spec TEXT NOT NULL,
    prompt TEXT,
    status TEXT NOT NULL CHECK(status IN ('active','paused','done','error')),
    related_task_id INTEGER,
    created_at TEXT NOT NULL,
    last_run_at TEXT,
    next_run_at TEXT,
    timezone TEXT,
    error TEXT
);
",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (title, created_at, updated_at, last_active_at) VALUES ('s', '2026-01-01', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let session_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO schedules (session_id, kind, spec, prompt, status, created_at, timezone) VALUES (?1, 'cron', '0 10 * * *', 'daily', 'active', '2026-01-01', 'Europe/Moscow')",
            params![session_id],
        )
        .unwrap();
        let schedule_id = conn.last_insert_rowid();
        conn.pragma_update(None, "user_version", 11).unwrap();

        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // The row survived the create/copy/drop/rename rebuild with its data.
        let spec: String = conn
            .query_row("SELECT spec FROM schedules WHERE id = ?1", params![schedule_id], |r| r.get(0))
            .unwrap();
        assert_eq!(spec, "0 10 * * *");

        // The rebuilt FK is SET NULL: deleting the session keeps the schedule.
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .unwrap();
        let schedule_session: Option<i64> = conn
            .query_row("SELECT session_id FROM schedules WHERE id = ?1", params![schedule_id], |r| r.get(0))
            .unwrap();
        assert_eq!(schedule_session, None);
    }

    #[test]
    fn migration_v13_recreates_vec_items_at_768_dims_and_clears_vectors() {
        crate::db::ensure_vec_extension();
        assert_eq!(crate::embed::EMBED_DIM, 768);
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v13 database: vec0 at float[512] with stale rows.
        conn.execute_batch("DROP TABLE vec_items; DROP TABLE file_vectors;")
            .unwrap();
        conn.execute_batch(
            "
CREATE TABLE file_vectors (
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
CREATE INDEX idx_file_vectors_rowid ON file_vectors(vec_rowid);
CREATE VIRTUAL TABLE vec_items USING vec0(
    project_id INTEGER PARTITION KEY,
    embedding float[512]
);
",
        )
        .unwrap();
        let blob512: Vec<u8> = vec![0.0f32; 512].iter().flat_map(|x| x.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![blob512],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_vectors (project_id, rel_path, kind, dim, backend, vec_rowid, updated_at) VALUES (1, 'a', 'text', 512, 'ort', 1, '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 12).unwrap();

        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let stale: i64 = conn
            .query_row("SELECT count(*) FROM file_vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stale, 0, "stale 512-d vector rows survived v13");
        let blob768: Vec<u8> = vec![0.0f32; 768].iter().flat_map(|x| x.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![blob768],
        )
        .unwrap();
        let wrong_dim = conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![blob512],
        );
        assert!(wrong_dim.is_err(), "512-d vector accepted into the 768-d vec0 table");
    }

    #[test]
    fn migration_v14_backfills_project_positions_from_id() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v14 database: projects without the position column.
        conn.execute_batch("ALTER TABLE projects DROP COLUMN position")
            .unwrap();
        for name in ["a", "b", "c"] {
            conn.execute(
                "INSERT INTO projects (name, path, created_at) VALUES (?1, ?2, '2026-01-01')",
                params![name, format!("/tmp/{name}")],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 13).unwrap();

        run_migrations(&conn).unwrap();

        let positions: Vec<f64> = {
            let mut stmt = conn
                .prepare("SELECT position FROM projects ORDER BY position, id")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY position, id").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(positions, vec![1.0, 2.0, 3.0]);
        assert_eq!(ids, vec![1, 2, 3], "backfill must keep id order stable");
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v15_creates_todos_with_scope_check_and_defaults() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        conn.execute(
            "INSERT INTO todos (scope_kind, scope_id, text, position, created_at, updated_at) VALUES ('session', 1, 'a', 1, '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let done: i64 = conn
            .query_row("SELECT done FROM todos WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(done, 0, "done must default to 0");

        let bad_kind = conn.execute(
            "INSERT INTO todos (scope_kind, scope_id, text, position, created_at, updated_at) VALUES ('bogus', 1, 'x', 1, '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(bad_kind.is_err(), "unknown scope kind accepted");

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_v16_drops_indexed_files_fk_for_the_notes_scope_id() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v16 database: indexed_files with the projects FK.
        conn.execute_batch("DROP TABLE indexed_files;").unwrap();
        conn.execute_batch(
            "
CREATE TABLE indexed_files (
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
",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO indexed_files (project_id, rel_path, name, kind, size, mtime, indexed_at) VALUES (1, 'a.md', 'a.md', 'text', 1, 1, '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 15).unwrap();

        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // The row survived the create/copy/drop/rename rebuild.
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM indexed_files WHERE project_id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1);

        // With FKs on, the sentinel notes scope id (-1, no projects row) is
        // now accepted; before v16 this insert failed.
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn.execute(
            "INSERT INTO indexed_files (project_id, rel_path, name, kind, size, mtime, indexed_at) VALUES (-1, 'нота.md', 'нота.md', 'text', 1, 1, '2026-01-01')",
            [],
        )
        .unwrap();
        let notes_rows: i64 = conn
            .query_row("SELECT count(*) FROM indexed_files WHERE project_id = -1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(notes_rows, 1);
    }

    #[test]
    fn migration_v17_widens_chats_check_keeps_transcript_and_notes_singleton() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v17 database: chats with the old CHECK (no 'notes').
        // FKs are off here (in-memory default), so the transcript survives
        // the drop/recreate of the parent table.
        conn.execute_batch("DROP TABLE chats;").unwrap();
        conn.execute_batch(
            "
CREATE TABLE chats (
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('project','harness')),
    title TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    session_id INTEGER REFERENCES sessions(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX uniq_chats_one_project_chat ON chats(project_id) WHERE kind = 'project';
",
        )
        .unwrap();
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
        let project_chat = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (?1, 'user', 'hi', '2026-01-01', 1)",
            params![project_chat],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 16).unwrap();

        // FKs ON during the migration: the rebuild must NOT cascade-delete
        // the transcript (the migration toggles FK enforcement itself).
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM messages WHERE chat_id = ?1", params![project_chat], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1, "v17 rebuild cascade-deleted the transcript");

        // 'notes' now passes the CHECK and the partial unique index keeps it
        // a singleton; project/harness rules are unchanged.
        conn.execute(
            "INSERT INTO chats (kind, created_at, updated_at) VALUES ('notes', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let second_notes = conn.execute(
            "INSERT INTO chats (kind, created_at, updated_at) VALUES ('notes', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(second_notes.is_err(), "second notes chat accepted");
        let dup_project = conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(dup_project.is_err(), "second project chat for the same project accepted");
        let bad_kind = conn.execute(
            "INSERT INTO chats (kind, created_at, updated_at) VALUES ('bogus', '2026-01-01', '2026-01-01')",
            [],
        );
        assert!(bad_kind.is_err());
    }

    #[test]
    fn migration_v18_adds_brief_columns_guarded() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let cols: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('projects')").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        for name in ["brief", "brief_updated_at"] {
            assert!(cols.iter().any(|c| c == name), "missing column {name}");
        }

        // Simulate an interrupted v18: user_version below 18 with the brief
        // column already present — the guarded ALTER must not fail.
        conn.execute_batch("ALTER TABLE projects DROP COLUMN brief_updated_at")
            .unwrap();
        conn.pragma_update(None, "user_version", 17).unwrap();
        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('projects')").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert!(cols.iter().any(|c| c == "brief_updated_at"));
    }

    #[test]
    fn migration_v19_creates_chat_index_tables_and_backfills_harness_messages() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v19 database: drop the three tables, add transcripts.
        conn.execute_batch(
            "DROP TABLE messages_fts; DROP TABLE message_vectors; DROP TABLE vec_messages;",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn.execute(
            "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
            [],
        )
        .unwrap();
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
        let harness_chat = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO chats (project_id, kind, created_at, updated_at) VALUES (1, 'project', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        let project_chat = conn.last_insert_rowid();
        for (chat, role, content) in [
            (harness_chat, "user", "ищем редизайн главной"),
            (harness_chat, "assistant", "готово"),
            (harness_chat, "tool", "редизайн tool"),
            (harness_chat, "user", "   "),
            (project_chat, "user", "редизайн проекта"),
        ] {
            conn.execute(
                "INSERT INTO messages (chat_id, role, content, created_at, position) VALUES (?1, ?2, ?3, '2026-01-01', 1)",
                params![chat, role, content],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 18).unwrap();

        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Only harness user/assistant turns with non-blank content were backfilled.
        let mut stmt = conn
            .prepare("SELECT message_id, session_id, content FROM messages_fts ORDER BY message_id")
            .unwrap();
        let rows: Vec<(i64, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|(_, sid, _)| *sid == session_id));
        let fts_hits: i64 = conn
            .query_row("SELECT count(*) FROM messages_fts WHERE messages_fts MATCH '\"редизайн\"'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_hits, 1, "cyrillic FTS backfill missed the harness turn");

        // vec_messages takes a 768-d vector under an explicit message rowid.
        let blob: Vec<u8> = vec![0.0f32; crate::embed::EMBED_DIM]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        conn.execute(
            "INSERT INTO vec_messages (rowid, embedding) VALUES (?1, ?2)",
            params![rows[0].0, blob],
        )
        .unwrap();
        let knn: i64 = conn
            .query_row(
                "SELECT count(*) FROM vec_messages WHERE embedding MATCH ?1 AND k = 1",
                params![blob],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(knn, 1);
        let wrong_dim = conn.execute(
            "INSERT INTO vec_messages (rowid, embedding) VALUES (99, ?1)",
            params![vec![0.0f32; 512].iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()],
        );
        assert!(wrong_dim.is_err(), "512-d vector accepted into the 768-d vec_messages");
    }

    #[test]
    fn migration_v20_rebuilds_memory_fts_with_memory_id_and_backfills() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Simulate a pre-v20 database: memory_fts with the v7 shape (content
        // only) and one memory row to backfill.
        conn.execute_batch("DROP TABLE memory_fts;").unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE memory_fts USING fts5(content, tokenize = 'unicode61');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory (content, tags, created_at, updated_at) VALUES ('remember this', '[\"x\"]', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 19).unwrap();

        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT memory_id, content FROM memory_fts")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![(1, "remember this".to_string())]);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('memory_vectors','vec_memory')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }
}
