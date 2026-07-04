use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Discord Channel Config
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfigRow {
    pub channel_id: String,
    #[serde(default)]
    pub agent_id: String, // "" = global
    pub guild_id: String,
    pub channel_name: String,
    pub readable: bool,
    pub writable: bool,
    pub whitelisted: bool,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_secs: Option<u64>,
    /// チャンネル単位のハートビート指示の上書き。空文字なら上書きなし。
    #[serde(default)]
    pub heartbeat_instructions: String,
}

/// グローバル設定（agent_id = ''）を取得する。
pub fn get_channel_config(conn: &Connection, channel_id: &str) -> Result<Option<ChannelConfigRow>> {
    get_channel_config_for_agent(conn, channel_id, "")
}

/// (channel_id, agent_id) で設定を取得する。agent_id = "" はグローバル設定。
pub fn get_channel_config_for_agent(
    conn: &Connection,
    channel_id: &str,
    agent_id: &str,
) -> Result<Option<ChannelConfigRow>> {
    let result = conn.query_row(
        "SELECT channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions
         FROM discord_channel_config WHERE channel_id = ?1 AND agent_id = ?2",
        params![channel_id, agent_id],
        |row| {
            Ok(ChannelConfigRow {
                channel_id: row.get(0)?,
                agent_id: row.get(1)?,
                guild_id: row.get(2)?,
                channel_name: row.get(3)?,
                readable: row.get(4)?,
                writable: row.get(5)?,
                whitelisted: row.get(6)?,
                heartbeat_enabled: row.get(7)?,
                heartbeat_interval_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                heartbeat_instructions: row.get(9)?,
            })
        },
    );

    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_channel_config(conn: &Connection, cfg: &ChannelConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO discord_channel_config (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(channel_id, agent_id) DO UPDATE SET
            guild_id = excluded.guild_id,
            channel_name = excluded.channel_name,
            readable = excluded.readable,
            writable = excluded.writable,
            whitelisted = excluded.whitelisted,
            heartbeat_enabled = excluded.heartbeat_enabled,
            heartbeat_interval_secs = excluded.heartbeat_interval_secs,
            heartbeat_instructions = excluded.heartbeat_instructions,
            updated_at = excluded.updated_at",
        params![
            cfg.channel_id,
            cfg.agent_id,
            cfg.guild_id,
            cfg.channel_name,
            cfg.readable,
            cfg.writable,
            cfg.whitelisted,
            cfg.heartbeat_enabled,
            cfg.heartbeat_interval_secs.map(|v| v as i64),
            cfg.heartbeat_instructions,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// グローバル設定（agent_id = ''）を削除する。
pub fn delete_channel_config(conn: &Connection, channel_id: &str) -> Result<bool> {
    delete_channel_config_for_agent(conn, channel_id, "")
}

/// (channel_id, agent_id) の設定を削除する。
pub fn delete_channel_config_for_agent(
    conn: &Connection,
    channel_id: &str,
    agent_id: &str,
) -> Result<bool> {
    let rows_affected = conn.execute(
        "DELETE FROM discord_channel_config WHERE channel_id = ?1 AND agent_id = ?2",
        rusqlite::params![channel_id, agent_id],
    )?;
    Ok(rows_affected > 0)
}

pub fn list_channel_configs_by_guild(
    conn: &Connection,
    guild_id: &str,
) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions
         FROM discord_channel_config WHERE guild_id = ?1 ORDER BY channel_name",
    )?;

    let rows = stmt.query_map(params![guild_id], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            agent_id: row.get(1)?,
            guild_id: row.get(2)?,
            channel_name: row.get(3)?,
            readable: row.get(4)?,
            writable: row.get(5)?,
            whitelisted: row.get(6)?,
            heartbeat_enabled: row.get(7)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            heartbeat_instructions: row.get(9)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 特定エージェントの設定一覧を取得する。agent_id = "" でグローバル設定。
pub fn list_channel_configs_by_agent(
    conn: &Connection,
    agent_id: &str,
) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions
         FROM discord_channel_config WHERE agent_id = ?1 ORDER BY channel_name",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            agent_id: row.get(1)?,
            guild_id: row.get(2)?,
            channel_name: row.get(3)?,
            readable: row.get(4)?,
            writable: row.get(5)?,
            whitelisted: row.get(6)?,
            heartbeat_enabled: row.get(7)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            heartbeat_instructions: row.get(9)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// whitelisted=true のチャンネルをすべて取得する。
pub fn list_whitelisted_channels(conn: &Connection) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions
         FROM discord_channel_config WHERE whitelisted = 1 ORDER BY channel_id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            agent_id: row.get(1)?,
            guild_id: row.get(2)?,
            channel_name: row.get(3)?,
            readable: row.get(4)?,
            writable: row.get(5)?,
            whitelisted: row.get(6)?,
            heartbeat_enabled: row.get(7)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            heartbeat_instructions: row.get(9)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// heartbeat_enabled=true のチャンネルをすべて取得する。
/// ハートビートを有効にすべきチャンネル一覧。
pub fn list_heartbeat_channels(conn: &Connection) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions
         FROM discord_channel_config WHERE heartbeat_enabled = 1 ORDER BY channel_id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            agent_id: row.get(1)?,
            guild_id: row.get(2)?,
            channel_name: row.get(3)?,
            readable: row.get(4)?,
            writable: row.get(5)?,
            whitelisted: row.get(6)?,
            heartbeat_enabled: row.get(7)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            heartbeat_instructions: row.get(9)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// チャンネルが読み取り可能か判定する。設定なし=true（デフォルト許可）。
pub fn is_channel_readable(conn: &Connection, channel_id: &str) -> bool {
    get_channel_config(conn, channel_id)
        .ok()
        .flatten()
        .map(|c| c.readable)
        .unwrap_or(true)
}

/// チャンネルが書き込み可能か判定する。設定なし=true（デフォルト許可）。
pub fn is_channel_writable(conn: &Connection, channel_id: &str) -> bool {
    get_channel_config(conn, channel_id)
        .ok()
        .flatten()
        .map(|c| c.writable)
        .unwrap_or(true)
}

/// チャンネルがホワイトリストに登録されているか判定する。設定なし=false（デフォルト拒否）。
pub fn is_channel_whitelisted(conn: &Connection, channel_id: &str) -> bool {
    get_channel_config(conn, channel_id)
        .ok()
        .flatten()
        .map(|c| c.whitelisted)
        .unwrap_or(false)
}

/// エージェント固有設定を優先し、なければグローバル設定にフォールバックして
/// チャンネルがホワイトリストに登録されているか判定する。設定なし=false。
pub fn is_channel_whitelisted_for_agent(
    conn: &Connection,
    channel_id: &str,
    agent_id: &str,
) -> bool {
    // エージェント固有設定を優先、なければグローバル設定
    if let Some(cfg) = get_channel_config_for_agent(conn, channel_id, agent_id)
        .ok()
        .flatten()
    {
        return cfg.whitelisted;
    }
    // グローバル設定へフォールバック
    get_channel_config_for_agent(conn, channel_id, "")
        .ok()
        .flatten()
        .map(|c| c.whitelisted)
        .unwrap_or(false)
}

/// エージェント固有設定を優先し、なければグローバル設定にフォールバックして
/// チャンネルが読み取り可能か判定する。設定なし=true（デフォルト許可）。
pub fn is_channel_readable_for_agent(conn: &Connection, channel_id: &str, agent_id: &str) -> bool {
    if let Some(cfg) = get_channel_config_for_agent(conn, channel_id, agent_id)
        .ok()
        .flatten()
    {
        return cfg.readable;
    }
    get_channel_config_for_agent(conn, channel_id, "")
        .ok()
        .flatten()
        .map(|c| c.readable)
        .unwrap_or(true)
}

/// エージェント固有設定を優先し、なければグローバル設定にフォールバックして
/// チャンネルが書き込み可能か判定する。設定なし=true（デフォルト許可）。
pub fn is_channel_writable_for_agent(conn: &Connection, channel_id: &str, agent_id: &str) -> bool {
    if let Some(cfg) = get_channel_config_for_agent(conn, channel_id, agent_id)
        .ok()
        .flatten()
    {
        return cfg.writable;
    }
    get_channel_config_for_agent(conn, channel_id, "")
        .ok()
        .flatten()
        .map(|c| c.writable)
        .unwrap_or(true)
}
