//! D-1096: the `interrupt` request (answered with `ok` once its effects are recorded).

use std::sync::Arc;

use tokio::net::unix::OwnedWriteHalf;

use crate::close::close_live;
use crate::error::{ErrorCode, GateError};
use crate::inbound::{binding_said_error, load_origin_row};
use crate::interrupt::interrupt_session;
use crate::protocol::{err_frame, ok_frame, write_json, Interrupt};
use crate::registry::ExtgateState;

type Writer = Arc<tokio::sync::Mutex<OwnedWriteHalf>>;

pub(super) async fn handle_interrupt(
    state: &Arc<ExtgateState>,
    writer: &Writer,
    instance_id: &str,
    identity: u64,
    interrupt: &Interrupt,
) -> Result<(), ()> {
    let response = match binding_session(state, instance_id, &interrupt.binding_id) {
        Ok((session_id, agent_id)) => {
            interrupt_session(state, &session_id, &agent_id, interrupt.delivered.clone()).await;
            ok_frame(&interrupt.id)
        }
        Err(e) if e.code == ErrorCode::StoreError => {
            close_live(
                state,
                Some(instance_id),
                Some(identity),
                ErrorCode::StoreError,
                Some(&interrupt.id),
                Some(writer),
            )
            .await;
            return Err(());
        }
        Err(e) => err_frame(&interrupt.id, e.code, e.detail.as_deref()),
    };
    if write_json(writer, &response).await.is_err() {
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(());
    }
    Ok(())
}

/// The session and agent of an acknowledged binding of this instance.
fn binding_session(
    state: &ExtgateState,
    instance_id: &str,
    binding_id: &str,
) -> Result<(String, String), GateError> {
    let acknowledged = {
        let reg = state.lock_registry()?;
        match reg.get(instance_id) {
            Some(live) => live.acknowledged.contains(binding_id),
            None => return Err(GateError::new(ErrorCode::InstanceUnknown)),
        }
    };
    // `binding_said_error` always errs: unknown, closed or not ready, as for a said.
    let not_ready = || match binding_said_error(state, instance_id, binding_id) {
        Err(e) => e,
        Ok(_) => GateError::new(ErrorCode::InstanceNotReady),
    };
    if !acknowledged {
        return Err(not_ready());
    }
    let conn = state
        .db
        .lock()
        .map_err(|e| GateError::store_logged("interrupt.db_lock", e))?;
    let Some(row) = load_origin_row(&conn, instance_id, binding_id)? else {
        drop(conn);
        return Err(not_ready());
    };
    match opencrab_db::queries::canonical_session_id(&conn, binding_id, &row.address) {
        Ok(Some(session_id)) => Ok((session_id, row.agent_id)),
        Ok(None) => Err(GateError::store_logged(
            "interrupt.session_missing",
            format!("no session for binding={binding_id}"),
        )),
        Err(error) => Err(GateError::store_logged("interrupt.session_lookup", error)),
    }
}
