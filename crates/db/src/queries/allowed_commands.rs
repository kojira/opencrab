use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

// ============================================
// AGENT ALLOWED COMMANDS
// ============================================

pub fn list_agent_allowed_commands(conn: &Connection, agent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT command FROM agent_allowed_commands WHERE agent_id = ?1 ORDER BY added_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn add_agent_allowed_command(
    conn: &Connection,
    agent_id: &str,
    command: &str,
    added_by: &str,
) -> Result<bool> {
    let id = format!("{}-{}", agent_id, command);
    let now = Utc::now().to_rfc3339();
    let rows_affected = conn.execute(
        "INSERT OR IGNORE INTO agent_allowed_commands (id, agent_id, command, added_by, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, agent_id, command, added_by, now],
    )?;
    Ok(rows_affected > 0)
}

pub fn remove_agent_allowed_command(
    conn: &Connection,
    agent_id: &str,
    command: &str,
) -> Result<bool> {
    let rows_affected = conn.execute(
        "DELETE FROM agent_allowed_commands WHERE agent_id = ?1 AND command = ?2",
        params![agent_id, command],
    )?;
    Ok(rows_affected > 0)
}
