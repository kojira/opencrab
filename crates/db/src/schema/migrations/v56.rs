use rusqlite::{Connection, OptionalExtension};

use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 56,
    description: "version internal co-agent relationships",
    up: migrate_versioned_co_agent_relationships,
}];

fn migrate_versioned_co_agent_relationships(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "trusted_co_agents")? {
        return Ok(());
    }
    if !column_exists(conn, "trusted_co_agents", "relationship_revision")? {
        conn.execute_batch(
            "ALTER TABLE trusted_co_agents ADD COLUMN relationship_revision INTEGER NOT NULL DEFAULT 1 CHECK(relationship_revision > 0);",
        )?;
    }
    if !column_exists(conn, "trusted_co_agents", "active")? {
        conn.execute_batch(
            "ALTER TABLE trusted_co_agents ADD COLUMN active INTEGER NOT NULL DEFAULT 1 CHECK(active IN (0,1));",
        )?;
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
