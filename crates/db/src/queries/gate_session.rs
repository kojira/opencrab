//! D-1099: core resolves a gateway's opaque session key to its own session.
//!
//! A gateway tags every frame with a session key it chooses. Core keeps `(instance, key) →
//! session` as its own internal record in `gate_bindings` (the row id is used only inside core,
//! for idempotency and delivery records). Nothing is registered beforehand and nothing is
//! closed: the first frame with a new key creates the record and the session.
//!
//! Existing rows keep their session (migrated rows have `session_id = key`, older rows
//! `extgate-<row id>`), so a conversation continues across the change.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::{get_session, insert_agent_session_in_tx, insert_session_in_tx};

/// Core's internal record for one `(instance, session key)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateSession {
    /// Internal row id (`gate_bindings.binding_id`). Never sent to a gateway.
    pub row_id: String,
    pub instance_id: String,
    pub session_key: String,
    pub session_id: String,
    pub agent_id: String,
}

/// Why [`resolve_gate_session_in_tx`] could not resolve the key.
#[derive(Debug)]
pub enum GateSessionError {
    /// The instance is unknown or deleted.
    UnknownInstance,
    Store(anyhow::Error),
}

impl From<rusqlite::Error> for GateSessionError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

impl From<anyhow::Error> for GateSessionError {
    fn from(e: anyhow::Error) -> Self {
        Self::Store(e)
    }
}

/// The internal row id of a new record: uuid5(instance, key) when the instance id is a UUID.
fn new_row_id(instance_id: &str, session_key: &str) -> String {
    match uuid::Uuid::parse_str(instance_id) {
        Ok(namespace) => uuid::Uuid::new_v5(&namespace, session_key.as_bytes()).to_string(),
        Err(_) => uuid::Uuid::new_v4().to_string(),
    }
}

fn instance_agent(conn: &Connection, instance_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT a.agent_id FROM gate_instances i
             JOIN agents a ON a.subject_id = i.subject_id
             WHERE i.instance_id = ?1 AND i.deleted_at IS NULL",
            [instance_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// The record of `(instance, key)` without writing: an open row first, else the latest one.
pub fn find_gate_session(
    conn: &Connection,
    instance_id: &str,
    session_key: &str,
) -> Result<Option<GateSession>> {
    let Some(agent_id) = instance_agent(conn, instance_id)? else {
        return Ok(None);
    };
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT binding_id, session_id FROM gate_bindings
             WHERE instance_id = ?1 AND session_key = ?2
             ORDER BY closed_at IS NOT NULL, created_at DESC, binding_id
             LIMIT 1",
            params![instance_id, session_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((row_id, stored)) = row else {
        return Ok(None);
    };
    let session_id = match stored {
        Some(session_id) => session_id,
        None => legacy_session_id(conn, &row_id, session_key)?,
    };
    Ok(Some(GateSession {
        row_id,
        instance_id: instance_id.to_string(),
        session_key: session_key.to_string(),
        session_id,
        agent_id,
    }))
}

/// A pre-v54 row without a stored session: `extgate-<row id>` if that session exists, else the
/// key itself.
fn legacy_session_id(conn: &Connection, row_id: &str, session_key: &str) -> Result<String> {
    let physical = format!("extgate-{row_id}");
    if get_session(conn, &physical)?.is_some() {
        return Ok(physical);
    }
    Ok(session_key.to_string())
}

/// Finds or creates the record of `(instance, key)` in the caller's transaction. A new record
/// uses the key itself as the session id; the session row and its membership are created when
/// missing. `title` names a newly created session (default: the key).
pub fn resolve_gate_session_in_tx(
    tx: &Transaction<'_>,
    instance_id: &str,
    session_key: &str,
    title: Option<&str>,
) -> std::result::Result<GateSession, GateSessionError> {
    if let Some(found) = find_gate_session(tx, instance_id, session_key)? {
        return Ok(found);
    }
    let agent_id = instance_agent(tx, instance_id)?.ok_or(GateSessionError::UnknownInstance)?;
    let session_id = session_key.to_string();
    let now = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    if get_session(tx, &session_id)?.is_none() {
        let now_rfc = chrono::DateTime::<Utc>::from_timestamp_nanos(now).to_rfc3339();
        insert_session_in_tx(tx, &session_id, title.unwrap_or(session_key), &now_rfc)?;
    }
    let member: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_sessions WHERE agent_id = ?1 AND session_id = ?2)",
        params![agent_id, session_id],
        |row| row.get(0),
    )?;
    if !member {
        insert_agent_session_in_tx(tx, &agent_id, &session_id)?;
    }
    let row_id = new_row_id(instance_id, session_key);
    tx.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at, closed_at, session_id)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        params![row_id, instance_id, session_key, now, session_id],
    )?;
    Ok(GateSession {
        row_id,
        instance_id: instance_id.to_string(),
        session_key: session_key.to_string(),
        session_id,
        agent_id,
    })
}

/// The record of an internal row id (for in-flight work that started from a frame).
pub fn gate_session_by_row(conn: &Connection, row_id: &str) -> Result<Option<GateSession>> {
    let row: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT instance_id, session_key, session_id FROM gate_bindings WHERE binding_id = ?1",
            [row_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((instance_id, session_key, stored)) = row else {
        return Ok(None);
    };
    let Some(agent_id) = instance_agent(conn, &instance_id)? else {
        return Ok(None);
    };
    let session_id = match stored {
        Some(session_id) => session_id,
        None => legacy_session_id(conn, row_id, &session_key)?,
    };
    Ok(Some(GateSession {
        row_id: row_id.to_string(),
        instance_id,
        session_key,
        session_id,
        agent_id,
    }))
}

/// Result of [`gate_session_for_session_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateSessionLookup {
    Match(GateSession),
    NotFound,
    /// More than one gateway record leads to the session; core does not pick one arbitrarily.
    Ambiguous,
}

/// The gateway record that leads to a core session (for turns that start without a frame, such
/// as timed fires). Records whose resolved session is `session_id` are candidates.
pub fn gate_session_for_session_id(
    conn: &Connection,
    session_id: &str,
) -> Result<GateSessionLookup> {
    let mut candidates: Vec<String> = Vec::new();
    {
        let mut statement = conn.prepare(
            "SELECT b.binding_id FROM gate_bindings b
             JOIN gate_instances i ON i.instance_id = b.instance_id
             WHERE i.deleted_at IS NULL
               AND (b.session_id = ?1 OR b.session_key = ?1 OR b.binding_id = ?2)
             ORDER BY b.closed_at IS NOT NULL, b.created_at DESC, b.binding_id",
        )?;
        let physical = session_id.strip_prefix("extgate-").unwrap_or("");
        let rows = statement
            .query_map(params![session_id, physical], |row| row.get::<_, String>(0))?;
        for row in rows {
            candidates.push(row?);
        }
    }
    let mut matches: Vec<GateSession> = Vec::new();
    for row_id in candidates {
        let Some(found) = gate_session_by_row(conn, &row_id)? else {
            continue;
        };
        if found.session_id != session_id {
            continue;
        }
        // One record per (instance, key): an older duplicate of the same key is not a second
        // route.
        if matches
            .iter()
            .any(|m| m.instance_id == found.instance_id && m.session_key == found.session_key)
        {
            continue;
        }
        matches.push(found);
    }
    Ok(match matches.len() {
        0 => GateSessionLookup::NotFound,
        1 => GateSessionLookup::Match(matches.remove(0)),
        _ => GateSessionLookup::Ambiguous,
    })
}

#[cfg(test)]
#[path = "gate_session_tests.rs"]
mod tests;
