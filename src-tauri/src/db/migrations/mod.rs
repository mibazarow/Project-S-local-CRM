// Ordered schema migration registry. SCHEMA_VERSION stays in db::schema (the
// public path the app uses) and the last registry target references it, so
// the table and the constant cannot drift apart. Each migration body lives in
// a version-range submodule; the bodies run when the stored user_version is
// below their target, exactly as the original hand-rolled if-chain did.
mod v01_10;
mod v11_20;
mod v21_30;
mod v31_40;
mod v41_50;

use crate::db::schema::SCHEMA_VERSION;
use crate::error::Result;
use rusqlite::Connection;

type Migration = fn(&Connection) -> Result<()>;

const MIGRATIONS: &[(u32, Migration)] = &[
    (1, v01_10::migrate_v1),
    (2, v01_10::migrate_v2),
    (3, v01_10::migrate_v3),
    (4, v01_10::migrate_v4),
    (5, v01_10::migrate_v5),
    (6, v01_10::migrate_v6),
    (7, v01_10::migrate_v7),
    (8, v01_10::migrate_v8),
    (9, v01_10::migrate_v9),
    (10, v01_10::migrate_v10),
    (11, v11_20::migrate_v11),
    (12, v11_20::migrate_v12),
    (13, v11_20::migrate_v13),
    (14, v11_20::migrate_v14),
    (15, v11_20::migrate_v15),
    (16, v11_20::migrate_v16),
    (17, v11_20::migrate_v17),
    (18, v11_20::migrate_v18),
    (19, v11_20::migrate_v19),
    (20, v11_20::migrate_v20),
    (21, v21_30::migrate_v21),
    (22, v21_30::migrate_v22),
    (23, v21_30::migrate_v23),
    (24, v21_30::migrate_v24),
    (25, v21_30::migrate_v25),
    (26, v21_30::migrate_v26),
    (27, v21_30::migrate_v27),
    (28, v21_30::migrate_v28),
    (29, v21_30::migrate_v29),
    (30, v21_30::migrate_v30),
    (31, v31_40::migrate_v31),
    (32, v31_40::migrate_v32),
    (33, v31_40::migrate_v33),
    (34, v31_40::migrate_v34),
    (35, v31_40::migrate_v35),
    (36, v31_40::migrate_v36),
    (37, v31_40::migrate_v37),
    (38, v31_40::migrate_v38),
    (39, v31_40::migrate_v39),
    (40, v31_40::migrate_v40),
    (SCHEMA_VERSION as u32, v41_50::migrate_v41),
];

pub fn run_migrations(conn: &Connection) -> Result<()> {
    crate::db::ensure_vec_extension();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (target, migrate) in MIGRATIONS {
        if version < *target as i64 {
            migrate(conn)?;
            conn.pragma_update(None, "user_version", *target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_indexes_and_are_idempotent() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let indexes: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 8);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_from_zero_yields_current_version_and_all_tables() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN (
                    'projects','tasks','indexed_files','files_fts','file_vectors','vec_items',
                    'chats','messages','summaries','sessions','schedules','memory','memory_fts',
                    'tasks_fts','message_attachments','todos','messages_fts','message_vectors',
                    'vec_messages','processes','trash','versions','undo_log','objectives',
                    'objective_steps','links','memory_vectors','vec_memory','skill_proposals',
                    'style_profile')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 30);
    }

    #[test]
    fn fresh_and_stepwise_upgraded_schema_are_identical() {
        crate::db::ensure_vec_extension();
        let fresh = Connection::open_in_memory().unwrap();
        run_migrations(&fresh).unwrap();

        let upgraded = Connection::open_in_memory().unwrap();
        for (target, migrate) in MIGRATIONS {
            migrate(&upgraded).unwrap();
            upgraded.pragma_update(None, "user_version", target).unwrap();
        }

        let dump = |conn: &Connection| -> Vec<(String, String, Option<String>)> {
            let mut stmt = conn
                .prepare(
                    "SELECT type, name, sql FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(dump(&fresh), dump(&upgraded));

        let version: i64 = upgraded
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn task_status_check_constraint_rejects_unknown_status() {
        crate::db::ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        conn
            .execute(
                "INSERT INTO projects (name, path, created_at) VALUES ('p', '/tmp/p', '2026-01-01')",
                [],
            )
            .unwrap();
        let bad = conn.execute(
            "INSERT INTO tasks (project_id, title, status, created_at) VALUES (1, 't', 'bogus', '2026-01-01')",
            [],
        );
        assert!(bad.is_err());
    }
}
