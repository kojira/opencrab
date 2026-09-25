use super::Migration;
use rusqlite::Connection;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 56,
    description: "add generic REST API principals",
    up: migrate_v56_api_principals,
}];

fn migrate_v56_api_principals(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_principals (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            permission TEXT NOT NULL,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            display_name TEXT NOT NULL DEFAULT '',
            UNIQUE(user_id, agent_id)
        );
        CREATE INDEX IF NOT EXISTS idx_api_principals_agent ON api_principals(agent_id);",
    )
}
