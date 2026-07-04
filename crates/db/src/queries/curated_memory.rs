use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// MEMORY: Curated
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedMemoryRow {
    pub id: String,
    pub agent_id: String,
    pub category: String,
    pub content: String,
    pub created_at: String,
}

pub fn upsert_curated_memory(conn: &Connection, memory: &CuratedMemoryRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_curated (id, agent_id, category, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, category) DO UPDATE SET
            content = excluded.content,
            updated_at = excluded.updated_at",
        params![
            memory.id,
            memory.agent_id,
            memory.category,
            memory.content,
            now,
            now,
        ],
    )?;
    Ok(())
}

/// Deletes a single curated memory entry by ID and agent_id.
/// Returns true if deleted, false if not found.
pub fn delete_curated_memory_entry(
    conn: &Connection,
    agent_id: &str,
    entry_id: &str,
) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM memory_curated WHERE id = ?1 AND agent_id = ?2",
        params![entry_id, agent_id],
    )?;
    Ok(deleted > 0)
}

pub fn get_curated_memories(
    conn: &Connection,
    agent_id: &str,
    category: &str,
) -> Result<Vec<CuratedMemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 AND category = ?2 ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map(params![agent_id, category], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn list_curated_memories(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CuratedMemoryRow>, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_curated WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 ORDER BY created_at ASC LIMIT ?2 OFFSET ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, limit, offset], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok((rows.collect::<std::result::Result<_, _>>()?, total))
}
