//! 接続 close: delivery rows remain pending for ordered reconnect replay.

use std::sync::Arc;

use crate::error::ErrorCode;
use crate::operation_calls::close_pending_invokes;
use crate::protocol::{err_frame, write_json};
use crate::registry::{ExtgateState, Pending};

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
    let (invokes, writer) = {
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
        // Say/utterance rows stay durable `sending` (the pending state) and are replayed after
        // a compatible hello. Invoke calls keep their existing request/response ambiguity rule.
        let mut invokes = Vec::new();
        for p in entry.pending.into_values() {
            if let Pending::Invoke { call_id, reply, .. } = p {
                invokes.push((call_id, reply));
            }
        }
        (invokes, entry.writer)
    };
    // pending invoke を indeterminate 化し、各 await へ Indeterminate を届ける（§7.4）。
    close_pending_invokes(state, invokes);
    let _ = writer;
}

/// halt 時に live transports を閉じる。Pending delivery rows remain durable for replay.
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
