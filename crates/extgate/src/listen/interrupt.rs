//! D-1096: the `interrupt` request (answered with `ok` once its effects are recorded).

use std::sync::Arc;

use tokio::net::unix::OwnedWriteHalf;

use crate::close::close_live;
use crate::error::{ErrorCode, GateError};
use crate::inbound::resolve;
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
    let response = match key_session(state, instance_id, &interrupt.session_key) {
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

/// The session and agent of a session key of this live instance (D-1099: resolved, not checked).
fn key_session(
    state: &ExtgateState,
    instance_id: &str,
    session_key: &str,
) -> Result<(String, String), GateError> {
    if !state.lock_registry()?.is_live(instance_id) {
        return Err(GateError::new(ErrorCode::InstanceUnknown));
    }
    let mut conn = state
        .db
        .lock()
        .map_err(|e| GateError::store_logged("interrupt.db_lock", e))?;
    let row = resolve(&mut conn, instance_id, session_key, None)?;
    Ok((row.session_id, row.agent_id))
}
