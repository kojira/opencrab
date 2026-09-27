use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// TRUSTED CO-AGENTS
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedCoAgentRow {
    pub id: String,
    pub agent_id: String,
    pub co_agent_id: String,
    pub allowed_actions: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

pub fn list_trusted_co_agents(conn: &Connection, agent_id: &str) -> Result<Vec<TrustedCoAgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, co_agent_id, allowed_actions, created_by, created_at
         FROM trusted_co_agents WHERE agent_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(TrustedCoAgentRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            co_agent_id: row.get(2)?,
            allowed_actions: row.get(3)?,
            created_by: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// `agent_id` にとって `co_agent_id` が信頼済み co-agent として登録されているか。

pub fn is_trusted_co_agent(conn: &Connection, agent_id: &str, co_agent_id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM trusted_co_agents WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

pub fn insert_trusted_co_agent(conn: &Connection, row: &TrustedCoAgentRow) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_co_agents (id, agent_id, co_agent_id, allowed_actions, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, co_agent_id) DO UPDATE SET
            allowed_actions = excluded.allowed_actions,
            created_by = excluded.created_by",
        params![
            row.id,
            row.agent_id,
            row.co_agent_id,
            row.allowed_actions,
            row.created_by,
            row.created_at,
        ],
    )?;
    Ok(())
}

// `update_trusted_co_agent_actions` は #490 で撤去した。唯一の呼び出し元だった
// co-agents API の PATCH（`allowed_actions` 更新）が無くなり、この列は権限判定に
// 使われないため書き換える経路も要らなくなった（列自体は互換のため残す）。

pub fn delete_trusted_co_agent(
    conn: &Connection,
    agent_id: &str,
    co_agent_id: &str,
) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM trusted_co_agents WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id],
    )?;
    Ok(deleted > 0)
}
