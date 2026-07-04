use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Discord Config
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDiscordConfigRow {
    pub agent_id: String,
    pub bot_token: String,
    pub owner_discord_id: String,
    pub enabled: bool,
}

pub fn upsert_agent_discord_config(conn: &Connection, cfg: &AgentDiscordConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_discord_config (agent_id, bot_token, owner_discord_id, enabled, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
            bot_token = excluded.bot_token,
            owner_discord_id = excluded.owner_discord_id,
            enabled = excluded.enabled,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.bot_token,
            cfg.owner_discord_id,
            cfg.enabled,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_agent_discord_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<AgentDiscordConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, bot_token, owner_discord_id, enabled
         FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentDiscordConfigRow {
                agent_id: row.get(0)?,
                bot_token: row.get(1)?,
                owner_discord_id: row.get(2)?,
                enabled: row.get(3)?,
            })
        },
    );

    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_agent_discord_config(conn: &Connection, agent_id: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(deleted > 0)
}

pub fn set_agent_discord_config_enabled(
    conn: &Connection,
    agent_id: &str,
    enabled: bool,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_discord_config SET enabled = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![enabled, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

pub fn patch_agent_discord_config(
    conn: &Connection,
    agent_id: &str,
    bot_token: Option<&str>,
    owner_discord_id: Option<&str>,
) -> Result<bool> {
    let updated = match (bot_token, owner_discord_id) {
        (Some(token), Some(owner)) => conn.execute(
            "UPDATE agent_discord_config SET bot_token = ?1, owner_discord_id = ?2, updated_at = ?3 WHERE agent_id = ?4",
            params![token, owner, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (Some(token), None) => conn.execute(
            "UPDATE agent_discord_config SET bot_token = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![token, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (None, Some(owner)) => conn.execute(
            "UPDATE agent_discord_config SET owner_discord_id = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![owner, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (None, None) => 0,
    };
    Ok(updated > 0)
}

pub fn list_enabled_agent_discord_configs(conn: &Connection) -> Result<Vec<AgentDiscordConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, bot_token, owner_discord_id, enabled
         FROM agent_discord_config WHERE enabled = 1",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(AgentDiscordConfigRow {
            agent_id: row.get(0)?,
            bot_token: row.get(1)?,
            owner_discord_id: row.get(2)?,
            enabled: row.get(3)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
