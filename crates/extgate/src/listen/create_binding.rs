use std::sync::Arc;

use rusqlite::TransactionBehavior;

use crate::error::{ErrorCode, GateError};
use crate::protocol::{err_frame, ok_frame, write_json, CreateBinding};
use crate::registry::ExtgateState;

use super::enqueue_bind;

pub async fn handle_create_binding(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    instance_id: &str,
    request: CreateBinding,
) -> Result<(), ()> {
    let result = persist(state, instance_id, &request);
    match result {
        Ok(binding_id) => {
            if write_json(writer, &ok_frame(&request.id)).await.is_err() {
                return Err(());
            }
            enqueue_bind(state, instance_id, &binding_id, &request.address).await;
            Ok(())
        }
        Err(error) if error.code == ErrorCode::StoreError => Err(()),
        Err(error) => write_json(writer, &err_frame(&request.id, error.code, None))
            .await
            .map_err(|_| ()),
    }
}

/// Returns the binding that now serves the address. D-1098: the instance's own binding for the
/// address is reused first (an open one as it is, a closed one reopened), so a channel that was
/// removed and is enabled again keeps its binding and conversation.
fn persist(
    state: &ExtgateState,
    instance_id: &str,
    request: &CreateBinding,
) -> Result<String, GateError> {
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let reused =
        opencrab_db::queries::reuse_instance_binding_in_tx(&tx, instance_id, &request.address)
            .map_err(|_| GateError::store())?;
    if let Some(binding_id) = reused.binding_id().map(str::to_string) {
        if opencrab_db::queries::injected_commit_failure() {
            return Err(GateError::store());
        }
        tx.commit().map_err(|_| GateError::store())?;
        return Ok(binding_id);
    }
    match opencrab_db::queries::create_gate_binding_in_tx(
        &tx,
        &request.binding_id,
        instance_id,
        &request.address,
        &request.session_theme,
        crate::now_nanos(),
    ) {
        Ok(()) => {}
        Err(opencrab_db::queries::CreateGateBindingError::Unknown) => {
            return Err(GateError::new(ErrorCode::InstanceUnknown));
        }
        Err(
            opencrab_db::queries::CreateGateBindingError::Conflict
            | opencrab_db::queries::CreateGateBindingError::AddressInUse
            | opencrab_db::queries::CreateGateBindingError::Closed,
        ) => {
            return Err(GateError::new(ErrorCode::BindingConflict));
        }
        Err(opencrab_db::queries::CreateGateBindingError::Store(_)) => {
            return Err(GateError::store());
        }
    }
    if opencrab_db::queries::injected_commit_failure() {
        return Err(GateError::store());
    }
    tx.commit().map_err(|_| GateError::store())?;
    Ok(request.binding_id.clone())
}
