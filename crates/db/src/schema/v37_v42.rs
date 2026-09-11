use rusqlite::Connection;

use super::helpers::column_exists;
use super::sql::{AGENT_SCHEDULES_SQL, SESSION_HEARTBEAT_CONFIG_SQL};

/// v37 migration: create the generic session heartbeat and schedule tables.
pub(super) fn migrate_v37_session_heartbeat(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SESSION_HEARTBEAT_CONFIG_SQL)?;
    conn.execute_batch(AGENT_SCHEDULES_SQL)
}

/// v38 migration: align schedule timestamps with the heartbeat vocabulary.
pub(super) fn migrate_v38_align_schedule_vocab(conn: &Connection) -> rusqlite::Result<()> {
    if column_exists(conn, "agent_schedules", "last_run_at")?
        && !column_exists(conn, "agent_schedules", "last_fired_at")?
    {
        conn.execute_batch(
            "ALTER TABLE agent_schedules RENAME COLUMN last_run_at TO last_fired_at;",
        )?;
    }
    if column_exists(conn, "agent_schedules", "next_run_at")? {
        conn.execute_batch("ALTER TABLE agent_schedules DROP COLUMN next_run_at;")?;
    }
    Ok(())
}
