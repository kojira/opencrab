use rusqlite::{Connection, OptionalExtension};

use super::super::sql::SESSION_HEARTBEAT_INSTRUCTIONS_SQL;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 55,
    description: "add composite generic heartbeat instructions",
    up: migrate_generic_heartbeat_instructions,
}];

fn migrate_generic_heartbeat_instructions(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SESSION_HEARTBEAT_INSTRUCTIONS_SQL)?;
    if table_exists(conn, "heartbeat_instructions_audit")?
        && !column_exists(conn, "heartbeat_instructions_audit", "session_id")?
    {
        conn.execute_batch("ALTER TABLE heartbeat_instructions_audit ADD COLUMN session_id TEXT;")?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(true),
    )
    .optional()
    .map(|value| value.unwrap_or(false))
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM pragma_table_info(?1) WHERE name=?2",
        [table, column],
        |_| Ok(true),
    )
    .optional()
    .map(|value| value.unwrap_or(false))
}
