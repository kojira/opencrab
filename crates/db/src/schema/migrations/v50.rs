use super::Migration;
use crate::schema::{column_exists, table_exists};

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 50,
    description: "converge shared channel and heartbeat audit storage on opaque identifiers",
    up: |conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS channel_config (
                channel_id TEXT NOT NULL,
                agent_id TEXT NOT NULL DEFAULT '',
                guild_id TEXT NOT NULL,
                channel_name TEXT NOT NULL DEFAULT '',
                readable INTEGER NOT NULL DEFAULT 1,
                writable INTEGER NOT NULL DEFAULT 1,
                whitelisted INTEGER NOT NULL DEFAULT 0,
                heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
                heartbeat_interval_secs INTEGER,
                heartbeat_instructions TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                PRIMARY KEY (channel_id, agent_id)
             );
             CREATE INDEX IF NOT EXISTS idx_channel_guild ON channel_config(guild_id);
             CREATE INDEX IF NOT EXISTS idx_channel_agent ON channel_config(agent_id);",
        )?;

        // The predecessor audit table has the same ten values in the same order. Rebuild it
        // without naming or interpreting the predecessor's transport-specific column.
        if table_exists(conn, "heartbeat_instructions_audit")?
            && !column_exists(conn, "heartbeat_instructions_audit", "caller_user_id")?
        {
            conn.execute_batch(
                "CREATE TABLE heartbeat_instructions_audit_v50 (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    agent_id TEXT NOT NULL,
                    scope TEXT NOT NULL,
                    channel_id TEXT,
                    caller_identity TEXT NOT NULL,
                    caller_user_id TEXT,
                    old_value TEXT,
                    new_value TEXT,
                    reason TEXT,
                    created_at TEXT NOT NULL
                 );
                 INSERT INTO heartbeat_instructions_audit_v50 SELECT * FROM heartbeat_instructions_audit;
                 DROP TABLE heartbeat_instructions_audit;
                 ALTER TABLE heartbeat_instructions_audit_v50 RENAME TO heartbeat_instructions_audit;
                 CREATE INDEX IF NOT EXISTS idx_heartbeat_instructions_audit_agent
                   ON heartbeat_instructions_audit(agent_id, created_at DESC);",
            )?;
        }
        Ok(())
    },
}];
