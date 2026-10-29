//! D-1098: `provision` before `hello`. A gateway declares its own generic instance and the
//! complete set of its bindings; core applies it in one transaction and answers `provisioned`.

use std::sync::Arc;

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::error::{ErrorCode, GateError};
use crate::ids::{config_digest_from_b64, now_nanos, session_id_for_binding};
use crate::protocol::{err_frame, write_json, Provision};
use crate::registry::ExtgateState;

/// Session title of a binding created by `provision` (same as the gate-admin reconciler used).
const BINDING_SESSION_TITLE: &str = "gateway session";

pub(super) async fn handle_provision(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    request: Provision,
) -> Result<(), ()> {
    let frame = match persist(state, &request) {
        Ok(snapshot) => snapshot,
        Err(error) => err_frame(&request.id, error.code, None),
    };
    write_json(writer, &frame).await.map_err(|_| ())
}

/// Applies the declaration. A live instance is refused (`instance_active`): the gateway stops its
/// connection first, provisions, then says `hello` again.
pub(crate) fn persist(state: &ExtgateState, request: &Provision) -> Result<Value, GateError> {
    let registry = state.lock_registry()?;
    if registry.is_live(&request.instance_id) {
        return Err(GateError::new(ErrorCode::InstanceActive));
    }
    let digest = config_digest_from_b64(&request.config_b64)?;
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let subject_id: i64 = tx
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id=?1 AND subject_id IS NOT NULL",
            [&request.agent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| GateError::store())?
        .ok_or_else(|| GateError::new(ErrorCode::SubjectUnknown))?;
    let now = now_nanos();
    apply_instance(&tx, request, subject_id, &digest, now)?;
    apply_bindings(&tx, request, now)?;
    let (revision, config_digest, enabled): (i64, String, i64) = tx
        .query_row(
            "SELECT revision, config_digest, enabled FROM gate_instances WHERE instance_id=?1",
            [&request.instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| GateError::store())?;
    tx.commit().map_err(|_| GateError::store())?;
    drop(registry);
    Ok(json!({
        "m": "provisioned",
        "id": request.id,
        "instance_id": request.instance_id,
        "revision": revision,
        "config_digest": config_digest,
        "enabled": enabled == 1,
        "bindings": request.bindings.iter().map(|binding| json!({
            "binding_id": binding.binding_id,
            "address": binding.address,
        })).collect::<Vec<_>>(),
    }))
}

fn apply_instance(
    tx: &rusqlite::Transaction<'_>,
    request: &Provision,
    subject_id: i64,
    digest: &str,
    now: i64,
) -> Result<(), GateError> {
    let existing: Option<(String, i64, i64, String, i64, Option<i64>)> = tx
        .query_row(
            "SELECT kind_id, subject_id, revision, config_b64, enabled, deleted_at
             FROM gate_instances WHERE instance_id=?1",
            [&request.instance_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(|_| GateError::store())?;
    match existing {
        None => {
            tx.execute(
                "INSERT INTO gate_instances (
                    instance_id, kind_id, subject_id, revision, enabled,
                    config_b64, config_digest, created_at, updated_at, deleted_at
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?7, NULL)",
                params![
                    request.instance_id,
                    request.kind_id,
                    subject_id,
                    i64::from(request.enabled),
                    request.config_b64,
                    digest,
                    now
                ],
            )
            .map_err(|_| GateError::store())?;
        }
        Some((kind_id, stored_subject, revision, config_b64, enabled, deleted_at)) => {
            if deleted_at.is_some() || kind_id != request.kind_id || stored_subject != subject_id {
                return Err(GateError::new(ErrorCode::InstanceConflict));
            }
            if config_b64 != request.config_b64 || enabled != i64::from(request.enabled) {
                let expected = u64::try_from(revision).map_err(|_| GateError::store())?;
                opencrab_db::queries::revise_gate_instance_in_tx(
                    tx,
                    &request.instance_id,
                    expected,
                    request.enabled,
                    &request.config_b64,
                    digest,
                    now,
                )
                .map_err(|_| GateError::store())?;
            }
        }
    }
    Ok(())
}

fn apply_bindings(
    tx: &rusqlite::Transaction<'_>,
    request: &Provision,
    now: i64,
) -> Result<(), GateError> {
    use opencrab_db::queries::{CoreBindingRequest, CoreBindingService, CreateGateBindingError};
    for binding in &request.bindings {
        let other_owner: Option<String> = tx
            .query_row(
                "SELECT instance_id FROM gate_bindings
                 WHERE address=?1 AND closed_at IS NULL AND instance_id<>?2",
                params![binding.address, request.instance_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| GateError::store())?;
        if other_owner.is_some() {
            return Err(GateError::new(ErrorCode::AddressInUse));
        }
        let session_id = session_id_for_binding(&binding.binding_id);
        CoreBindingService::create_in_tx(
            tx,
            &CoreBindingRequest {
                binding_id: &binding.binding_id,
                instance_id: &request.instance_id,
                address: &binding.address,
                session_id: &session_id,
                session_title: BINDING_SESSION_TITLE,
                now,
            },
        )
        .map_err(|error| match error {
            CreateGateBindingError::AddressInUse => GateError::new(ErrorCode::AddressInUse),
            CreateGateBindingError::Unknown => GateError::new(ErrorCode::InstanceConflict),
            CreateGateBindingError::Conflict | CreateGateBindingError::Closed => {
                GateError::new(ErrorCode::BindingConflict)
            }
            CreateGateBindingError::Store(_) => GateError::store(),
        })?;
    }
    let mut open = tx
        .prepare("SELECT binding_id FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL")
        .map_err(|_| GateError::store())?;
    let open_ids = open
        .query_map([&request.instance_id], |row| row.get::<_, String>(0))
        .map_err(|_| GateError::store())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| GateError::store())?;
    drop(open);
    for binding_id in open_ids {
        if !request.bindings.iter().any(|b| b.binding_id == binding_id) {
            tx.execute(
                "UPDATE gate_bindings SET closed_at=?2 WHERE binding_id=?1",
                params![binding_id, now],
            )
            .map_err(|_| GateError::store())?;
        }
    }
    Ok(())
}
