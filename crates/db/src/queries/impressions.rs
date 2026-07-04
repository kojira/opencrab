use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Impressions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpressionRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub target_id: String,
    pub target_name: String,
    pub personality: String,
    pub communication_style: String,
    pub recent_behavior: String,
    pub agreement: String,
    pub notes: String,
    pub last_updated_turn: i32,
}

pub fn upsert_impression(conn: &Connection, imp: &ImpressionRow) -> Result<()> {
    conn.execute(
        "INSERT INTO impressions (id, agent_id, session_id, target_id, target_name, personality, communication_style, recent_behavior, agreement, notes, last_updated_turn, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(agent_id, session_id, target_id) DO UPDATE SET
            personality = excluded.personality,
            communication_style = excluded.communication_style,
            recent_behavior = excluded.recent_behavior,
            agreement = excluded.agreement,
            notes = excluded.notes,
            last_updated_turn = excluded.last_updated_turn,
            updated_at = excluded.updated_at",
        params![
            imp.id,
            imp.agent_id,
            imp.session_id,
            imp.target_id,
            imp.target_name,
            imp.personality,
            imp.communication_style,
            imp.recent_behavior,
            imp.agreement,
            imp.notes,
            imp.last_updated_turn,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_impressions(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Vec<ImpressionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, target_id, target_name, personality, communication_style, recent_behavior, agreement, notes, last_updated_turn
         FROM impressions WHERE agent_id = ?1 AND session_id = ?2",
    )?;

    let rows = stmt.query_map(params![agent_id, session_id], |row| {
        Ok(ImpressionRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            target_id: row.get(3)?,
            target_name: row.get(4)?,
            personality: row.get(5)?,
            communication_style: row.get(6)?,
            recent_behavior: row.get(7)?,
            agreement: row.get(8)?,
            notes: row.get(9)?,
            last_updated_turn: row.get(10)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
