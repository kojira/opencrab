//! D-1099: a frame's session key → core's session (find or create, no pre-registration).

use opencrab_db::queries::{resolve_gate_session_in_tx, GateSession, GateSessionError};
use rusqlite::{Connection, TransactionBehavior};

use crate::error::{ErrorCode, GateError};

/// Core's record for one session key of a live instance.
#[derive(Clone, Debug)]
pub(crate) struct OriginRow {
    pub(crate) instance_id: String,
    /// Internal row id: used only inside core (idempotency, delivery records).
    pub(crate) row_id: String,
    pub(crate) session_key: String,
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
}

impl From<GateSession> for OriginRow {
    fn from(found: GateSession) -> Self {
        Self {
            instance_id: found.instance_id,
            row_id: found.row_id,
            session_key: found.session_key,
            session_id: found.session_id,
            agent_id: found.agent_id,
        }
    }
}

fn map_error(error: GateSessionError) -> GateError {
    match error {
        GateSessionError::UnknownInstance => GateError::new(ErrorCode::InstanceUnknown),
        GateSessionError::Store(cause) => GateError::store_logged("session.resolve", cause),
    }
}

/// Finds or creates the record inside the caller's transaction.
pub(crate) fn resolve_in_tx(
    tx: &rusqlite::Transaction<'_>,
    instance_id: &str,
    session_key: &str,
    title: Option<&str>,
) -> Result<OriginRow, GateError> {
    resolve_gate_session_in_tx(tx, instance_id, session_key, title)
        .map(OriginRow::from)
        .map_err(map_error)
}

/// Finds or creates the record in its own transaction.
pub(crate) fn resolve(
    conn: &mut Connection,
    instance_id: &str,
    session_key: &str,
    title: Option<&str>,
) -> Result<OriginRow, GateError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| GateError::store_logged("session.tx_begin", e))?;
    let row = resolve_in_tx(&tx, instance_id, session_key, title)?;
    tx.commit()
        .map_err(|e| GateError::store_logged("session.tx_commit", e))?;
    Ok(row)
}
