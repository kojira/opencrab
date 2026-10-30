//! D-1085: `provisional` and `provisional_cancel` requests (both answered with `ok`).

use std::sync::Arc;

use opencrab_actions::AgentRuntime;
use tokio::net::unix::OwnedWriteHalf;

use crate::close::close_live;
use crate::error::ErrorCode;
use crate::protocol::{err_frame, ok_frame, write_json, Provisional, ProvisionalCancel};
use crate::provisional::run::start_provisional;
use crate::registry::ExtgateState;

type Writer = Arc<tokio::sync::Mutex<OwnedWriteHalf>>;

pub(super) async fn handle_provisional<R: AgentRuntime>(
    state: &Arc<ExtgateState>,
    runtime: &R,
    writer: &Writer,
    instance_id: &str,
    identity: u64,
    provisional: &Provisional,
) -> Result<(), ()> {
    let response = match start_provisional(state, runtime, instance_id, provisional).await {
        Ok(()) => ok_frame(&provisional.id),
        Err(e) if e.code == ErrorCode::StoreError => {
            close_live(
                state,
                Some(instance_id),
                Some(identity),
                ErrorCode::StoreError,
                Some(&provisional.id),
                Some(writer),
            )
            .await;
            return Err(());
        }
        Err(e) => err_frame(&provisional.id, e.code, e.detail.as_deref()),
    };
    respond(state, writer, instance_id, identity, &response).await
}

/// Idempotent. The `ok` is written after the run is aborted, so no partial of it follows.
pub(super) async fn handle_provisional_cancel(
    state: &Arc<ExtgateState>,
    writer: &Writer,
    instance_id: &str,
    identity: u64,
    cancel: &ProvisionalCancel,
) -> Result<(), ()> {
    let row_id = state.lock_registry().ok().and_then(|reg| {
        reg.get(instance_id)
            .filter(|live| live.identity == identity)
            .map(|_| ())
    });
    if row_id.is_some() {
        let found = state.db.lock().ok().and_then(|conn| {
            opencrab_db::queries::find_gate_session(&conn, instance_id, &cancel.session_key)
                .ok()
                .flatten()
        });
        if let Some(found) = found {
            if let Some(task) = state
                .provisionals
                .remove(&found.row_id, &cancel.provisional_id)
            {
                let _ = task.await;
            }
        }
    }
    respond(state, writer, instance_id, identity, &ok_frame(&cancel.id)).await
}

async fn respond(
    state: &Arc<ExtgateState>,
    writer: &Writer,
    instance_id: &str,
    identity: u64,
    response: &serde_json::Value,
) -> Result<(), ()> {
    if write_json(writer, response).await.is_err() {
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
