use super::Migration;
use rusqlite::{Connection, OptionalExtension};

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 57,
    description: "add generic declarative binding authority",
    up: migrate_binding_authority,
}];

fn migrate_binding_authority(conn: &Connection) -> rusqlite::Result<()> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM pragma_table_info('gate_instances') WHERE name='binding_authority'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        conn.execute_batch(
            "ALTER TABLE gate_instances ADD COLUMN binding_authority TEXT NOT NULL DEFAULT 'runtime'
                 CHECK(binding_authority IN ('runtime', 'declarative'));",
        )?;
    }
    Ok(())
}
