pub mod schema;
mod migrations;

use crate::error::Result;
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

// Registers sqlite-vec process-wide; SQLite dedupes by entry point, so
// calling this on every connection open is cheap and idempotent.
pub fn ensure_vec_extension() {
    // transmute matches the crate docs; the annotated target type would be
    // the full sqlite3_extension_init pointer signature.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

// Test-only instrumentation: counts db::open calls on the armed thread so a
// test can prove a call opens exactly one connection (S1.2). Thread-local, so
// parallel test threads never interfere with each other.
#[cfg(test)]
pub(crate) mod open_counter {
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<u32> = const { Cell::new(0) };
    }

    pub(crate) fn arm() {
        COUNT.with(|c| c.set(0));
        ARMED.with(|a| a.set(true));
    }

    pub(crate) fn stop() -> u32 {
        ARMED.with(|a| a.set(false));
        COUNT.with(|c| c.take())
    }

    pub(super) fn note() {
        ARMED.with(|a| {
            if a.get() {
                COUNT.with(|c| c.set(c.get() + 1));
            }
        });
    }
}

pub fn open(db_path: &Path) -> Result<Connection> {
    #[cfg(test)]
    open_counter::note();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ensure_vec_extension();
    let conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    schema::run_migrations(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::temp_test_dir;
    use rusqlite::params;

    fn f32_blob(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn vec_version_works_and_vec0_knn_orders_by_distance() {
        ensure_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        let version: String = conn.query_row("SELECT vec_version()", [], |r| r.get(0)).unwrap();
        assert!(version.starts_with('v'));

        schema::run_migrations(&conn).unwrap();
        let mut a = vec![0.0f32; crate::embed::EMBED_DIM];
        let mut b = vec![0.0f32; crate::embed::EMBED_DIM];
        a[0] = 1.0;
        b[1] = 1.0;
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![f32_blob(&a)],
        )
        .unwrap();
        let row_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO vec_items (project_id, embedding) VALUES (1, ?1)",
            params![f32_blob(&b)],
        )
        .unwrap();

        let mut stmt = conn
            .prepare("SELECT rowid, distance FROM vec_items WHERE embedding MATCH ?1 AND k = 2 AND project_id = 1 ORDER BY distance")
            .unwrap();
        let hits: Vec<(i64, f64)> = stmt
            .query_map(params![f32_blob(&a)], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(hits[0].0, row_a);
        assert!(hits[0].1.abs() < 1e-6);
        // unit vectors at 90 degrees are sqrt(2) apart in L2
        assert!((hits[1].1 - std::f64::consts::SQRT_2).abs() < 1e-3);
    }

    #[test]
    fn open_applies_pragmas_and_migrations_and_is_idempotent() {
        let dir = temp_test_dir("db");
        let file = dir.join("project-s.db");

        let conn = open(&file).unwrap();
        let again = open(&file).unwrap();
        drop(again);

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);

        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('projects','tasks')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);

        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
