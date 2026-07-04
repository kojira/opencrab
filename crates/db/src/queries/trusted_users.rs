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

pub fn update_trusted_co_agent_actions(
    conn: &Connection,
    agent_id: &str,
    co_agent_id: &str,
    allowed_actions: Option<&str>,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE trusted_co_agents SET allowed_actions = ?3 WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id, allowed_actions],
    )?;
    Ok(updated > 0)
}

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

// ============================================
// TrustedDiscordUser
// ============================================

#[derive(Debug, Clone)]
pub struct TrustedDiscordUserRow {
    pub id: String,
    pub discord_user_id: String,
    pub agent_id: String,
    pub permission: String,
    pub created_by: String,
    pub created_at: String,
    /// ロスター表示用の名前（ピアレビュアー一覧等）。空文字可。
    pub display_name: String,
}

fn trusted_user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrustedDiscordUserRow> {
    Ok(TrustedDiscordUserRow {
        id: row.get(0)?,
        discord_user_id: row.get(1)?,
        agent_id: row.get(2)?,
        permission: row.get(3)?,
        created_by: row.get(4)?,
        created_at: row.get(5)?,
        display_name: row.get(6)?,
    })
}

const TRUSTED_USER_COLUMNS: &str =
    "id, discord_user_id, agent_id, permission, created_by, created_at, display_name";

pub fn get_trusted_user(
    conn: &Connection,
    discord_user_id: &str,
    agent_id: &str,
) -> Option<TrustedDiscordUserRow> {
    conn.query_row(
        &format!(
            "SELECT {TRUSTED_USER_COLUMNS} \
             FROM trusted_discord_users WHERE discord_user_id = ?1 AND agent_id = ?2"
        ),
        [discord_user_id, agent_id],
        trusted_user_from_row,
    )
    .ok()
}

pub fn list_trusted_users(conn: &Connection, agent_id: &str) -> Result<Vec<TrustedDiscordUserRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRUSTED_USER_COLUMNS} \
         FROM trusted_discord_users WHERE agent_id = ?1 ORDER BY created_at ASC"
    ))?;
    let rows = stmt.query_map([agent_id], trusted_user_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[allow(clippy::too_many_arguments)]
pub fn add_trusted_user(
    conn: &Connection,
    id: &str,
    agent_id: &str,
    discord_user_id: &str,
    permission: &str,
    created_by: &str,
    created_at: &str,
    display_name: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_discord_users (id, discord_user_id, agent_id, permission, created_by, created_at, display_name) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        [id, discord_user_id, agent_id, permission, created_by, created_at, display_name],
    )?;
    Ok(())
}

/// このエージェントのピアレビュアー（permission='co_agent' の trusted user）一覧。
/// プロンプトのロスター表示と reviewer 解決の両方がこれを使う（選定ロジックの一元化）。
pub fn list_co_agent_reviewers(
    conn: &Connection,
    agent_id: &str,
) -> Result<Vec<TrustedDiscordUserRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TRUSTED_USER_COLUMNS} \
         FROM trusted_discord_users WHERE agent_id = ?1 AND permission = 'co_agent' \
         ORDER BY created_at ASC"
    ))?;
    let rows = stmt.query_map([agent_id], trusted_user_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn update_trusted_user_display_name(
    conn: &Connection,
    id: &str,
    display_name: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE trusted_discord_users SET display_name = ?2 WHERE id = ?1",
        [id, display_name],
    )?;
    Ok(n > 0)
}

pub fn update_trusted_user_permission(
    conn: &Connection,
    id: &str,
    permission: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE trusted_discord_users SET permission = ?2 WHERE id = ?1",
        [id, permission],
    )?;
    Ok(n > 0)
}

pub fn remove_trusted_user(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM trusted_discord_users WHERE id = ?1", [id])?;
    Ok(n > 0)
}

pub fn is_trusted_user(conn: &Connection, discord_user_id: &str, agent_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM trusted_discord_users WHERE discord_user_id = ?1 AND agent_id = ?2",
        [discord_user_id, agent_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

pub fn trusted_user_count(conn: &Connection, agent_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM trusted_discord_users WHERE agent_id = ?1",
        [agent_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}
