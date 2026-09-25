use crate::canonical;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SOURCE_VERSION: u64 = 56;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Null,
    Text(String),
    Integer(i64),
    Blob(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub table: String,
    pub columns: Vec<(String, Cell)>,
    pub fingerprint: String,
}

pub const CONCRETE_TABLES: &[(&str, &[&str])] = &[
    (
        "trusted_users",
        &[
            "id",
            "user_id",
            "agent_id",
            "permission",
            "created_by",
            "created_at",
            "display_name",
            "platform",
        ],
    ),
    (
        "channel_config",
        &[
            "channel_id",
            "agent_id",
            "guild_id",
            "channel_name",
            "readable",
            "writable",
            "whitelisted",
            "heartbeat_enabled",
            "heartbeat_interval_secs",
            "heartbeat_instructions",
            "updated_at",
        ],
    ),
    (
        "session_watches",
        &[
            "id",
            "session_id",
            "agent_id",
            "interval_secs",
            "filter_json",
            "created_at",
        ],
    ),
    (
        "agent_discord_config",
        &[
            "agent_id",
            "bot_token",
            "owner_discord_id",
            "enabled",
            "updated_at",
            "bot_user_id",
        ],
    ),
    (
        "agent_nostr_config",
        &[
            "agent_id",
            "secret_key",
            "relays_json",
            "filter_json",
            "enabled",
            "updated_at",
            "owner_pubkey",
            "self_pubkey",
        ],
    ),
];

const REQUIRED_TABLES: &[&str] = &[
    "agents",
    "sessions",
    "agent_sessions",
    "gate_instances",
    "gate_bindings",
    "deliveries",
    "trusted_users",
    "channel_config",
    "session_watches",
    "session_heartbeat_config",
    "session_heartbeat_instructions",
    "api_principals",
];

pub fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

pub fn validate(conn: &Connection) -> Result<Vec<SourceRow>> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version == SOURCE_VERSION as i64,
        "core schema must be exactly 56"
    );
    for table in REQUIRED_TABLES {
        ensure!(table_exists(conn, table)?, "missing required table {table}");
    }
    let mut rows = Vec::new();
    for (table, columns) in CONCRETE_TABLES {
        let exists = table_exists(conn, table)?;
        if matches!(*table, "agent_discord_config" | "agent_nostr_config") && !exists {
            continue;
        }
        ensure!(exists, "missing required source table {table}");
        let actual = table_columns(conn, table)?;
        ensure!(
            actual == *columns,
            "source table {table} has unexpected shape"
        );
        rows.extend(read_rows(conn, table, columns)?);
    }
    rows.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
    Ok(rows)
}

pub fn fingerprint(table: &str, columns: &[(String, Cell)]) -> String {
    let mut bytes = b"opencrab/s8/source-row/v1\0".to_vec();
    lp(&mut bytes, table.as_bytes());
    bytes.extend_from_slice(&SOURCE_VERSION.to_be_bytes());
    bytes.extend_from_slice(&(columns.len() as u32).to_be_bytes());
    for (name, value) in columns {
        lp(&mut bytes, name.as_bytes());
        match value {
            Cell::Null => bytes.push(0),
            Cell::Text(value) => {
                bytes.push(1);
                lp(&mut bytes, value.as_bytes());
            }
            Cell::Integer(value) => {
                bytes.push(1);
                lp(&mut bytes, value.to_string().as_bytes());
            }
            Cell::Blob(value) => {
                bytes.push(1);
                lp(&mut bytes, value);
            }
        }
    }
    canonical::hex(&Sha256::digest(bytes))
}

pub fn file_sha256(path: &Path) -> Result<String> {
    Ok(canonical::hex(&Sha256::digest(std::fs::read(path)?)))
}

pub fn fingerprint_set_sha256(rows: &[&SourceRow]) -> Result<String> {
    let mut hashes = rows
        .iter()
        .map(|row| row.fingerprint.as_str())
        .collect::<Vec<_>>();
    hashes.sort_unstable();
    let mut bytes = Vec::with_capacity(hashes.len() * 32);
    for hash in hashes {
        ensure!(canonical::is_sha256(hash), "invalid fingerprint");
        for index in (0..64).step_by(2) {
            bytes.push(u8::from_str_radix(&hash[index..index + 2], 16)?);
        }
    }
    Ok(canonical::hex(&Sha256::digest(bytes)))
}

fn read_rows(conn: &Connection, table: &str, columns: &[&str]) -> Result<Vec<SourceRow>> {
    let sql = format!("SELECT {} FROM {table} ORDER BY rowid", columns.join(","));
    let mut statement = conn.prepare(&sql)?;
    let mapped = statement.query_map([], |row| {
        let mut values = Vec::with_capacity(columns.len());
        for (index, name) in columns.iter().enumerate() {
            let cell = match row.get_ref(index)? {
                ValueRef::Null => Cell::Null,
                ValueRef::Integer(value) => Cell::Integer(value),
                ValueRef::Text(value) => Cell::Text(String::from_utf8_lossy(value).into_owned()),
                ValueRef::Blob(value) => Cell::Blob(value.to_vec()),
                ValueRef::Real(_) => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        index,
                        (*name).into(),
                        rusqlite::types::Type::Real,
                    ))
                }
            };
            values.push(((*name).to_string(), cell));
        }
        Ok(values)
    })?;
    mapped
        .map(|result| {
            let columns = result?;
            let fingerprint = fingerprint(table, &columns);
            Ok(SourceRow {
                table: table.into(),
                columns,
                fingerprint,
            })
        })
        .collect()
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<&'static str>> {
    let expected = CONCRETE_TABLES
        .iter()
        .find(|(name, _)| *name == table)
        .context("known source table")?
        .1;
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let actual = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual.len() != expected.len() {
        bail!("source table {table} has unexpected column count");
    }
    for (actual, expected) in actual.iter().zip(expected.iter()) {
        ensure!(actual == expected, "source table {table} column mismatch");
    }
    Ok(expected.to_vec())
}

fn lp(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

impl SourceRow {
    pub fn text(&self, name: &str) -> Result<&str> {
        match self
            .columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, value)| value)
        {
            Some(Cell::Text(value)) => Ok(value),
            _ => bail!("{name} is not TEXT"),
        }
    }

    pub fn integer(&self, name: &str) -> Result<i64> {
        match self
            .columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, value)| value)
        {
            Some(Cell::Integer(value)) => Ok(*value),
            _ => bail!("{name} is not INTEGER"),
        }
    }

    pub fn optional_integer(&self, name: &str) -> Result<Option<i64>> {
        match self
            .columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, value)| value)
        {
            Some(Cell::Integer(value)) => Ok(Some(*value)),
            Some(Cell::Null) => Ok(None),
            _ => bail!("{name} has invalid type"),
        }
    }
}
