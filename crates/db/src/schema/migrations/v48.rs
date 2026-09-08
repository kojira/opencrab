use rusqlite::Connection;

use super::super::Migration;

pub(super) const MIGRATIONS: &[Migration] = &[Migration {
    version: 48,
    description: "store provider-executed tool history on llm logs",
    up: migrate_v48_provider_tool_history,
}];

fn migrate_v48_provider_tool_history(conn: &Connection) -> rusqlite::Result<()> {
    if !super::super::column_exists(conn, "llm_logs", "provider_tool_history")? {
        conn.execute_batch(
            "ALTER TABLE llm_logs
             ADD COLUMN provider_tool_history TEXT NOT NULL DEFAULT '{}';",
        )?;
    }
    Ok(())
}
