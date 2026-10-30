//! Provider-neutral Said ingress.

#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{AgentRuntime, CallerIdentity};
use rusqlite::{params, TransactionBehavior};

use crate::error::{ErrorCode, GateError};
use crate::protocol::{Said, SaidCaller};
use crate::registry::ExtgateState;

mod attachments;
mod record;
mod session;
mod turn;

use attachments::{materialize, promote_local_files, remove_local_files, remove_promoted_files};
pub use record::record_agent_prelude;
pub(crate) use record::{agent_prelude_row, inbound_speech_metadata, seq_for_origin};
use record::{existing_seq, next_seq, record_inbound};
pub(crate) use session::{resolve, resolve_in_tx};
use turn::enqueue_turn;

pub struct SaidOutcome {
    pub seq: Option<i64>,
    /// The core session the said's key resolved to (D-1099).
    pub session_id: String,
}

/// D-1085: a said that starts no turn never promotes its provisional run.
fn discard_provisional(state: &ExtgateState, row_id: &str, said: &Said) {
    if let Some(provisional_id) = &said.provisional_id {
        state.provisionals.discard(row_id, provisional_id);
    }
}

pub(crate) fn asserted_caller(caller: &SaidCaller) -> CallerIdentity {
    match caller {
        SaidCaller::Owner => CallerIdentity::Owner,
        SaidCaller::Agent => CallerIdentity::Agent,
        SaidCaller::CoAgent { agent_id } => CallerIdentity::CoAgent {
            agent_id: agent_id.clone(),
        },
        SaidCaller::TrustedUser => CallerIdentity::TrustedUser,
    }
}

pub fn process_said<R: AgentRuntime>(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    said: &Said,
    runtime: &R,
) -> Result<SaidOutcome, GateError> {
    if !state.lock_registry()?.is_live(instance_id) {
        return Err(GateError::new(ErrorCode::InstanceUnknown));
    }

    let mut conn = state
        .db
        .lock()
        .map_err(|e| GateError::store_logged("said.db_lock", e))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| GateError::store_logged("said.tx_begin", e))?;

    // D-1099: the key is resolved (or created) here; nothing is registered beforehand.
    let row = resolve_in_tx(&tx, instance_id, &said.session_key, None)?;
    let session_id = row.session_id.clone();
    if let Some(seq) = existing_seq(&tx, &row.row_id, &said.origin)? {
        tx.commit().map_err(|_| GateError::store())?;
        remove_local_files(state, said);
        return Ok(SaidOutcome {
            seq: Some(seq),
            session_id,
        });
    }

    #[cfg(any(test, feature = "extgate-probe"))]
    state
        .probe
        .accept_inbound_count
        .fetch_add(1, Ordering::SeqCst);

    let materialized = materialize(state, said)?;
    let said = &materialized;
    // The gateway decided who may speak here; core records what it was given.
    let recorded_rows =
        record_inbound(&tx, &session_id, &row, said, &said.text).and_then(|said_row| {
            match &said.agent_prelude {
                Some(prelude) => record_agent_prelude(&tx, &session_id, &row.agent_id, prelude)
                    .map(|prelude_row| (said_row, Some(prelude_row))),
                None => Ok((said_row, None)),
            }
        });
    let recorded_rows = match recorded_rows {
        Ok(ids) => ids,
        Err(_) => {
            let _ = tx.rollback();
            return Err(GateError::with_detail(
                ErrorCode::StoreError,
                "said.record_inbound",
            ));
        }
    };

    let seq = next_seq(&tx, &row.row_id)?;
    tx.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES (?1, ?2, ?3)",
        params![row.row_id, said.origin, seq],
    )
    .map_err(|error| GateError::store_logged("said.external_origins_insert", error))?;
    let enqueue = said.start_turn;
    if enqueue && !state.turn_queues.has_room(&session_id) {
        let dropped = state.turn_queues.note_dropped();
        let _ = tx.rollback();
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .turn_queue_dropped
            .fetch_add(1, Ordering::SeqCst);
        tracing::warn!(
            session_id,
            dropped_total = dropped,
            "extgate turn queue full"
        );
        discard_provisional(state, &row.row_id, said);
        return Ok(SaidOutcome {
            seq: None,
            session_id,
        });
    }
    let promoted = match promote_local_files(state, said) {
        Ok(paths) => paths,
        Err(error) => {
            let _ = tx.rollback();
            return Err(error);
        }
    };
    if let Err(error) = tx.commit() {
        remove_promoted_files(&promoted);
        return Err(GateError::store_logged("said.tx_commit", error));
    }
    drop(conn);

    if enqueue {
        let (said_row, prelude_row) = recorded_rows;
        let own_rows = std::iter::once(said_row).chain(prelude_row).collect();
        enqueue_turn(
            Arc::clone(state),
            runtime.clone(),
            &row,
            said,
            &session_id,
            said.system_context.as_deref().unwrap_or(""),
            Some(seq),
            // D-1093: core は返信先を決めない。said の origin は失敗通知（turn_failed）の相関に
            // だけ渡す。返信はモデルが対象つきの返信操作を明示的に呼んだときだけ起きる。
            Some(said.origin.as_str()),
            own_rows,
        );
    } else {
        discard_provisional(state, &row.row_id, said);
    }
    Ok(SaidOutcome {
        seq: Some(seq),
        session_id,
    })
}
