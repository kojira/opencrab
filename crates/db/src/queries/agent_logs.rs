use anyhow::Result;
use rusqlite::Connection;

#[allow(unused_imports)]
use super::*;

// ============================================================
// agent_logs
// ============================================================

#[derive(Debug, Clone)]
pub struct AgentLogRow {
    pub id: String,
    pub agent_id: Option<String>,
    pub level: String,
    pub context: String,
    pub message: String,
    pub created_at: Option<String>,
}

pub fn insert_agent_log(conn: &Connection, row: &AgentLogRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_logs (id, agent_id, level, context, message, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, datetime('now')))",
        rusqlite::params![
            row.id,
            row.agent_id,
            row.level,
            row.context,
            row.message,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn list_agent_logs(
    conn: &Connection,
    agent_id: Option<&str>,
    level_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<AgentLogRow>> {
    let mut rows = Vec::new();
    match (agent_id, level_filter) {
        (Some(aid), Some(lvl)) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs WHERE agent_id=?1 AND level=?2 ORDER BY created_at DESC LIMIT ?3",
            )?;
            for r in stmt.query_map(rusqlite::params![aid, lvl, limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
        (Some(aid), None) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs WHERE agent_id=?1 ORDER BY created_at DESC LIMIT ?2",
            )?;
            for r in stmt.query_map(rusqlite::params![aid, limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
        (None, Some(lvl)) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs WHERE level=?1 ORDER BY created_at DESC LIMIT ?2",
            )?;
            for r in stmt.query_map(rusqlite::params![lvl, limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
        (None, None) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs ORDER BY created_at DESC LIMIT ?1",
            )?;
            for r in stmt.query_map(rusqlite::params![limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
    }
    Ok(rows)
}
