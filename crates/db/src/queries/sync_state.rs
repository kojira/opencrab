use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// IMPORT SYNC STATE
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStateRow {
    pub id: String,
    pub agent_id: String,
    pub source_dir: String,
    pub file_type: String,
    pub file_name: String,
    pub content_hash: String,
    pub synced_at: String,
    pub created_at: String,
}

pub fn get_sync_state(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    file_name: &str,
) -> Result<Option<SyncStateRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, source_dir, file_type, file_name, content_hash, synced_at, created_at
         FROM import_sync_state
         WHERE agent_id = ?1 AND source_dir = ?2 AND file_name = ?3",
        params![agent_id, source_dir, file_name],
        |row| {
            Ok(SyncStateRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                source_dir: row.get(2)?,
                file_type: row.get(3)?,
                file_name: row.get(4)?,
                content_hash: row.get(5)?,
                synced_at: row.get(6)?,
                created_at: row.get(7)?,
            })
        },
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_sync_state(conn: &Connection, row: &SyncStateRow) -> Result<()> {
    conn.execute(
        "INSERT INTO import_sync_state (id, agent_id, source_dir, file_type, file_name, content_hash, synced_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(agent_id, source_dir, file_name) DO UPDATE SET
            content_hash = excluded.content_hash,
            synced_at = excluded.synced_at,
            file_type = excluded.file_type",
        params![
            row.id,
            row.agent_id,
            row.source_dir,
            row.file_type,
            row.file_name,
            row.content_hash,
            row.synced_at,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn list_sync_states(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SyncStateRow>, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM import_sync_state WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, agent_id, source_dir, file_type, file_name, content_hash, synced_at, created_at
         FROM import_sync_state WHERE agent_id = ?1
         ORDER BY synced_at DESC LIMIT ?2 OFFSET ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, limit, offset], |row| {
        Ok(SyncStateRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            source_dir: row.get(2)?,
            file_type: row.get(3)?,
            file_name: row.get(4)?,
            content_hash: row.get(5)?,
            synced_at: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;

    Ok((rows.collect::<std::result::Result<_, _>>()?, total))
}

pub fn delete_sync_states_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM import_sync_state WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

pub fn get_latest_sync_at(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT MAX(synced_at) FROM import_sync_state WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(val) => Ok(val),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
