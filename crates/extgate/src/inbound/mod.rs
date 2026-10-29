//! Provider-neutral Said ingress.

use std::cell::Cell;
#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    accept_inbound, AgentRuntime, CallerIdentity, InboundLookups, InboundWork,
    NormalizedInboundEvent,
};
use rusqlite::{params, TransactionBehavior};

use crate::error::{ErrorCode, GateError};
use crate::protocol::{Said, SaidCaller};
use crate::registry::ExtgateState;

mod attachments;
mod binding;
mod record;
mod turn;

use attachments::{materialize, promote_local_files, remove_local_files, remove_promoted_files};
pub(crate) use binding::{binding_said_error, load_origin_row};
pub(crate) use binding::{resolve_binding_context, BindingContext};
pub(crate) use record::{agent_prelude_row, inbound_speech_metadata, seq_for_origin};
pub use record::{channel_whitelisted, record_agent_prelude};
use record::{existing_seq, next_seq, record_inbound};
use turn::enqueue_turn;

pub struct SaidOutcome {
    pub seq: Option<i64>,
}

/// D-1085: a said that starts no turn never promotes its provisional run.
fn discard_provisional(state: &ExtgateState, said: &Said) {
    if let Some(provisional_id) = &said.provisional_id {
        state.provisionals.discard(&said.binding_id, provisional_id);
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
    {
        let reg = state.lock_registry()?;
        match reg.get(instance_id) {
            Some(live) if live.acknowledged.contains(&said.binding_id) => {}
            Some(_) => {
                drop(reg);
                return binding_said_error(state, instance_id, &said.binding_id);
            }
            None => return Err(GateError::new(ErrorCode::InstanceUnknown)),
        }
    }

    let mut conn = state
        .db
        .lock()
        .map_err(|e| GateError::store_logged("said.db_lock", e))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| GateError::store_logged("said.tx_begin", e))?;

    let row = match load_origin_row(&tx, instance_id, &said.binding_id)? {
        Some(row) => row,
        None => {
            let _ = tx.rollback();
            drop(conn);
            return binding_said_error(state, instance_id, &said.binding_id);
        }
    };
    if let Some(seq) = existing_seq(&tx, &said.binding_id, &said.origin)? {
        tx.commit().map_err(|_| GateError::store())?;
        remove_local_files(state, said);
        return Ok(SaidOutcome { seq: Some(seq) });
    }

    #[cfg(any(test, feature = "extgate-probe"))]
    state
        .probe
        .accept_inbound_count
        .fetch_add(1, Ordering::SeqCst);

    let session_id =
        match opencrab_db::queries::canonical_session_id(&tx, &said.binding_id, &row.address) {
            Ok(Some(id)) => id,
            Ok(None) => {
                let _ = tx.rollback();
                return Err(GateError::store_logged(
                    "said.session_missing",
                    format!(
                        "no session for binding={} nor address={}",
                        said.binding_id, row.address
                    ),
                ));
            }
            Err(error) => {
                let _ = tx.rollback();
                return Err(GateError::store_logged("said.session_lookup", error));
            }
        };
    let materialized = materialize(state, said)?;
    let said = &materialized;
    let recorded = Cell::new(false);
    let record_failed = Cell::new(false);
    let run_after = Cell::new(false);
    // D-1085: the memory_sessions ids of this said's own rows (inbound, agent prelude).
    let recorded_rows = Cell::new(None::<(i64, Option<i64>)>);
    let agent_ids = [row.agent_id.clone()];
    let event = NormalizedInboundEvent {
        sender_id: said.author_id.as_str(),
        channel_id: row.address.as_str(),
        // External bindings are channel-like. The gateway owns platform-specific DM admission.
        guild_id: instance_id,
    };
    let work = [InboundWork {
        event,
        has_content: !said.text.is_empty() || !said.attachments.is_empty(),
    }];
    let caller = asserted_caller(&said.caller);
    let resolve = |_sender: &str, _agents: &[String], _owner: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .lookup_resolve_count
            .fetch_add(1, Ordering::SeqCst);
        caller.clone()
    };
    let dm_any = |_sender: &str, _agents: &[String], _owner: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .lookup_dm_any_count
            .fetch_add(1, Ordering::SeqCst);
        true
    };
    let dm_one = |_sender: &str, _agent: &str, _owner: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        state.probe.lookup_dm_count.fetch_add(1, Ordering::SeqCst);
        true
    };
    let whitelist = |channel_id: &str, agent_id: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        {
            state.probe.lookup_wl_count.fetch_add(1, Ordering::SeqCst);
            if let Ok(override_value) = state.probe.whitelist_override.lock() {
                if let Some(value) = *override_value {
                    return value;
                }
            }
        }
        channel_whitelisted(&tx, agent_id, instance_id, channel_id)
    };
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &dm_any,
        dm_allowed: &dm_one,
        channel_whitelisted: &whitelist,
    };

    let accepted = accept_inbound(
        &work,
        "",
        &agent_ids,
        &lookups,
        |_, admitted| {
            if admitted.admitted_agent_ids.contains(&row.agent_id) {
                let recorded_said = record_inbound(&tx, &session_id, &row, said, &said.text)
                    .and_then(|said_row| match &said.agent_prelude {
                        Some(prelude) => {
                            record_agent_prelude(&tx, &session_id, &row.agent_id, prelude)
                                .map(|prelude_row| (said_row, Some(prelude_row)))
                        }
                        None => Ok((said_row, None)),
                    });
                match recorded_said {
                    Ok(ids) => {
                        recorded.set(true);
                        recorded_rows.set(Some(ids));
                    }
                    Err(_) => record_failed.set(true),
                }
            }
        },
        |_, admitted, _| {
            if recorded.get() && admitted.admitted_agent_ids.contains(&row.agent_id) {
                run_after.set(true);
            }
        },
    );

    if record_failed.get() {
        let _ = tx.rollback();
        return Err(GateError::with_detail(
            ErrorCode::StoreError,
            "said.record_inbound",
        ));
    }
    if accepted.is_err() || !recorded.get() {
        let _ = tx.rollback();
        remove_local_files(state, said);
        discard_provisional(state, said);
        return Ok(SaidOutcome { seq: None });
    }

    let seq = next_seq(&tx, &said.binding_id)?;
    tx.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES (?1, ?2, ?3)",
        params![said.binding_id, said.origin, seq],
    )
    .map_err(|error| GateError::store_logged("said.external_origins_insert", error))?;
    let enqueue = run_after.get() && said.start_turn;
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
        discard_provisional(state, said);
        return Ok(SaidOutcome { seq: None });
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
        let own_rows = recorded_rows
            .get()
            .map(|(said_row, prelude_row)| std::iter::once(said_row).chain(prelude_row).collect())
            .unwrap_or_default();
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
        discard_provisional(state, said);
    }
    Ok(SaidOutcome { seq: Some(seq) })
}
