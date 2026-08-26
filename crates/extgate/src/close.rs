//! 接続 close: pending say を indeterminate にし、同じ identity だけ消す。

use std::sync::Arc;

use crate::delivery::mark_indeterminate;
use crate::error::ErrorCode;
use crate::protocol::{err_frame, write_json};
use crate::registry::ExtgateState;

pub async fn close_live(
    state: &Arc<ExtgateState>,
    instance_id: Option<&str>,
    identity: Option<u64>,
    reason: ErrorCode,
    request_id: Option<&str>,
    writer: Option<&tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    if let Some(w) = writer {
        if let Some(id) = request_id {
            let _ = write_json(w, &err_frame(id, reason, None)).await;
        } else {
            tracing::error!(code = reason.as_str(), "gate close without request id");
        }
    }

    let Some(instance_id) = instance_id else {
        return;
    };
    let (says, writer) = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => {
                state.halt();
                return;
            }
        };
        let identity = match identity {
            Some(i) => i,
            None => match reg.get(instance_id) {
                Some(e) => e.identity,
                None => return,
            },
        };
        let Some(entry) = reg.remove_if_identity(instance_id, identity) else {
            return;
        };
        let says: Vec<String> = entry
            .pending
            .values()
            .filter_map(|p| p.delivery_id().map(str::to_string))
            .collect();
        (says, entry.writer)
    };
    if let Err(e) = mark_indeterminate(state, &says) {
        tracing::error!(code = e.code.as_str(), "pending say terminal write failed");
        state.halt();
    }
    let _ = writer;
}

/// halt 時に全 live の pending say を terminal 化する。
pub async fn close_all_lives(state: &Arc<ExtgateState>, reason: ErrorCode) {
    let targets = match state.lock_registry() {
        Ok(reg) => reg.identities(),
        Err(_) => return,
    };
    for (instance_id, identity) in targets {
        close_live(
            state,
            Some(&instance_id),
            Some(identity),
            reason,
            None,
            None,
        )
        .await;
    }
}
