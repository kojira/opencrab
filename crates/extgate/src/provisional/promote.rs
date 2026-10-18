//! D-1085: promotion of a provisional run by its committed said (inside the turn's serialized
//! section, before delivery).

use std::sync::Arc;

use super::{ProvisionalInputs, TakenProvisional};
use crate::partial_stream::acknowledged_writer;
use crate::protocol::{provisional_result_frame, write_json};
use crate::registry::ExtgateState;

/// What the said must match.
pub(crate) struct PromotionKey<'a> {
    pub instance_id: &'a str,
    pub binding_id: &'a str,
    pub provisional_id: &'a str,
    pub session_id: &'a str,
    pub inputs: ProvisionalInputs,
    /// The said's own memory_sessions rows (inbound, agent prelude).
    pub own_rows: &'a [i64],
}

/// Takes the provisional run when the said repeats its inputs exactly and the session gained
/// no row since the run's snapshot other than `own_rows`. Otherwise aborts it. Writes
/// `provisional_result` either way.
pub(crate) async fn take_promotion(
    state: &Arc<ExtgateState>,
    key: PromotionKey<'_>,
) -> Option<TakenProvisional> {
    let promoted = match state.provisionals.take(key.binding_id, key.provisional_id) {
        Some(taken) if matches(state, &key, &taken) => Some(taken),
        Some(taken) => {
            taken.task.abort();
            let _ = taken.task.await;
            None
        }
        None => None,
    };
    tracing::info!(
        binding_id = %key.binding_id,
        provisional_id = %key.provisional_id,
        promoted = promoted.is_some(),
        "provisional promotion decided"
    );
    write_result(
        state,
        key.instance_id,
        key.binding_id,
        key.provisional_id,
        promoted.is_some(),
    )
    .await;
    promoted
}

/// A said whose turn does not run: aborts its provisional run and reports `promoted:false`.
pub(crate) async fn reject(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    provisional_id: &str,
) {
    state.provisionals.discard(binding_id, provisional_id);
    write_result(state, instance_id, binding_id, provisional_id, false).await;
}

fn matches(state: &ExtgateState, key: &PromotionKey<'_>, taken: &TakenProvisional) -> bool {
    if taken.session_id != key.session_id || taken.inputs != key.inputs {
        return false;
    }
    let Ok(conn) = state.db.lock() else {
        return false;
    };
    matches!(
        opencrab_db::queries::session_has_rows_after(
            &conn,
            key.session_id,
            taken.high_water,
            key.own_rows,
        ),
        Ok(false)
    )
}

async fn write_result(
    state: &ExtgateState,
    instance_id: &str,
    binding_id: &str,
    provisional_id: &str,
    promoted: bool,
) {
    let Some(writer) = acknowledged_writer(state, instance_id, binding_id) else {
        return;
    };
    let frame = provisional_result_frame(binding_id, provisional_id, promoted);
    if let Err(e) = write_json(&writer, &frame).await {
        tracing::warn!(
            code = e.code.as_str(),
            "provisional_result write failed; ignored"
        );
    }
}
