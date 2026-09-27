//! Test-only readback for the frozen v50 channel migration fixture.
use anyhow::Result;
use rusqlite::{params, Connection};

#[derive(Debug)]
pub struct ChannelConfigRow {
    pub agent_id: String,
    pub guild_id: String,
    pub channel_name: String,
    pub readable: bool,
    pub writable: bool,
    pub whitelisted: bool,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_secs: Option<u64>,
    pub heartbeat_instructions: String,
}

pub fn get_channel_config(conn: &Connection, channel_id: &str) -> Result<Option<ChannelConfigRow>> {
    get_channel_config_for_agent(conn, channel_id, "")
}

pub fn get_channel_config_for_agent(
    conn: &Connection,
    channel_id: &str,
    agent_id: &str,
) -> Result<Option<ChannelConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id,guild_id,channel_name,readable,writable,whitelisted,heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions
         FROM channel_config WHERE channel_id=?1 AND agent_id=?2",
        params![channel_id, agent_id],
        |row| {
            Ok(ChannelConfigRow {
                agent_id: row.get(0)?,
                guild_id: row.get(1)?,
                channel_name: row.get(2)?,
                readable: row.get(3)?,
                writable: row.get(4)?,
                whitelisted: row.get(5)?,
                heartbeat_enabled: row.get(6)?,
                heartbeat_interval_secs: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                heartbeat_instructions: row.get(8)?,
            })
        },
    );
    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
