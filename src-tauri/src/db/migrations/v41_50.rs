// Migrations v41+: per-chat working directory. A harness chat can be bound to
// a folder on disk; the agent's file tools then resolve relative paths against
// it and treat that folder as their territory. NULL keeps the previous
// behaviour (the app-wide working dir).
use crate::error::Result;
use rusqlite::Connection;

/// chats.working_dir — absolute path chosen in the chat header, NULL when the
/// chat uses the app-wide working dir. Guarded per column so a re-run after an
/// interrupted migration does not re-ALTER.
pub(super) fn migrate_v41(conn: &Connection) -> Result<()> {
    let has_working_dir: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('chats') WHERE name = 'working_dir'",
        [],
        |r| r.get(0),
    )?;
    if has_working_dir == 0 {
        conn.execute("ALTER TABLE chats ADD COLUMN working_dir TEXT", [])?;
    }
    Ok(())
}
