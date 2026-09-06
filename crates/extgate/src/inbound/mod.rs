//! said → accept_inbound。V3 §6.2 / §7.2。

use std::cell::Cell;
#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    accept_inbound, AgentRuntime, CallerIdentity, InboundLookups, InboundWork,
    NormalizedInboundEvent, PrivilegeFire, WatchAccept,
};
use opencrab_db::queries::{
    get_session_policy_json, get_session_watch, TRUSTED_PLATFORM_EXTGATE, TRUSTED_PLATFORM_NOSTR,
};
use rusqlite::{params, TransactionBehavior};

use crate::bundle::{apply_bundle_member, NostrBundleAdmit};
use crate::error::{ErrorCode, GateError};
use crate::protocol::Said;
use crate::registry::{ExtgateState, NostrHeldTurn, NostrSaidDecision, NostrWatchSets};
use crate::ResolveCallerFn;

mod binding;
mod bundle_turn;
mod nostr_profile;
mod record;
#[cfg(test)]
mod tests;
mod turn;

use binding::{binding_said_error, load_origin_row};
pub(crate) use binding::{resolve_binding_context, BindingContext};
use bundle_turn::{conclude_unrecorded_bundle, finish_bundle, BundleCtx};
use nostr_profile::{
    fire_nostr_relay, inbound_kind_label, nostr_prompt_suffix, recorded_said_text,
};
pub(crate) use record::seq_for_origin;
pub use record::{channel_whitelisted, dm_allowed};
use record::{existing_seq, next_seq, record_inbound};
use turn::{enqueue_turn, fire_held_turns};

pub struct SaidOutcome {
    pub seq: Option<i64>,
}

pub fn process_said<R: AgentRuntime>(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    said: &Said,
    resolve_caller: ResolveCallerFn,
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
        Some(r) => r,
        None => {
            let _ = tx.rollback();
            drop(conn);
            return binding_said_error(state, instance_id, &said.binding_id);
        }
    };

    if let Some(seq) = existing_seq(&tx, &said.binding_id, &said.origin)? {
        tx.commit().map_err(|_| GateError::store())?;
        return Ok(SaidOutcome { seq: Some(seq) });
    }

    #[cfg(any(test, feature = "extgate-probe"))]
    state
        .probe
        .accept_inbound_count
        .fetch_add(1, Ordering::SeqCst);

    let recorded = Cell::new(false);
    let record_failed = Cell::new(false);
    let run_after = Cell::new(false);
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
            Err(e) => {
                let _ = tx.rollback();
                return Err(GateError::store_logged("said.session_lookup", e));
            }
        };
    let ctx = BundleCtx {
        state,
        runtime,
        resolve_caller,
        row: &row,
        said,
        session_id: &session_id,
    };
    let mut nostr_watch: Option<(i64, bool)> = None;
    let mut bundle_admit: Option<NostrBundleAdmit> = None;
    let mut bundle_dropped = false;
    if row.kind_id == "nostr" {
        match state.admit_nostr_said(&row.agent_id, &said.author_id, &said.text)? {
            NostrSaidDecision::Drop { bundle: None } => {
                tx.commit().map_err(|_| GateError::store())?;
                return Ok(SaidOutcome { seq: None });
            }
            NostrSaidDecision::Drop { bundle: Some(b) } => {
                bundle_admit = Some(b);
                bundle_dropped = true;
            }
            NostrSaidDecision::Accept {
                watch_id,
                immediate,
                bundle,
            } => {
                bundle_admit = bundle;
                nostr_watch = watch_id.map(|id| (id, immediate));
            }
        }
    }
    if bundle_dropped {
        let Some(admit) = bundle_admit.as_ref() else {
            let _ = tx.rollback();
            return Err(GateError::store());
        };
        let applied = apply_bundle_member(&tx, &said.binding_id, admit, false)?;
        return finish_bundle(tx, &ctx, applied, None);
    }
    let agent_ids = [row.agent_id.clone()];
    let event = NormalizedInboundEvent {
        sender_id: said.author_id.as_str(),
        channel_id: row.address.as_str(),
        guild_id: row.kind_id.as_str(),
    };
    let work = [InboundWork {
        event,
        has_content: !said.text.is_empty() || !said.attachments.is_empty(),
        // Defect B（QC #10）: watch 車線経由の said は accept_inbound の権限デバウンス判定
        // （watch_hold_interval_secs / AGREED_IMMEDIATE_KINDS）で kind ラベルを見る。ここを一律
        // "said" にすると owner/followee のメンション・リプライ・リアクションでも即時扱いにならず
        // interval 分だけ保留される。実 nostr kind からラベルを導出して即応判定を機能させる。
        kind_label: inbound_kind_label(&row, said),
        author_key: said.author_id.as_str(),
    }];

    let resolve = |sender: &str, agents: &[String], owner: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .lookup_resolve_count
            .fetch_add(1, Ordering::SeqCst);
        let agent_id = agents.first().map(String::as_str).unwrap_or("");
        let platform = if row.kind_id == "nostr" {
            TRUSTED_PLATFORM_NOSTR
        } else {
            TRUSTED_PLATFORM_EXTGATE
        };
        resolve_caller(&tx, platform, &[sender], agent_id, owner)
    };
    let dm_any = |sender: &str, agents: &[String], owner: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .lookup_dm_any_count
            .fetch_add(1, Ordering::SeqCst);
        agents
            .iter()
            .any(|agent_id| dm_allowed(&tx, sender, agent_id, owner))
    };
    let dm_one = |sender: &str, agent_id: &str, owner: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        state.probe.lookup_dm_count.fetch_add(1, Ordering::SeqCst);
        dm_allowed(&tx, sender, agent_id, owner)
    };
    let wl = |channel_id: &str, agent_id: &str| {
        #[cfg(any(test, feature = "extgate-probe"))]
        {
            state.probe.lookup_wl_count.fetch_add(1, Ordering::SeqCst);
            if let Ok(g) = state.probe.whitelist_override.lock() {
                if let Some(v) = *g {
                    return v;
                }
            }
        }
        channel_whitelisted(&tx, agent_id, instance_id, channel_id)
    };
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &dm_any,
        dm_allowed: &dm_one,
        channel_whitelisted: &wl,
    };

    let watch_row = match nostr_watch {
        Some((id, _)) => match get_session_watch(&tx, id) {
            Ok(Some(w)) if w.session_id == row.address => Some(w),
            Ok(_) | Err(_) => {
                let _ = tx.rollback();
                return Err(GateError::store());
            }
        },
        None => None,
    };
    let policy = match &watch_row {
        Some(w) => match get_session_policy_json(&tx, &w.session_id) {
            Ok(Some(p)) => p,
            Ok(None) | Err(_) => {
                let _ = tx.rollback();
                return Err(GateError::store());
            }
        },
        None => String::new(),
    };
    let watch_sets = if watch_row.is_some() {
        match state.nostr_watch_sets_for(&row.agent_id) {
            Some(sets) => sets,
            None => {
                let _ = tx.rollback();
                return Err(GateError::store());
            }
        }
    } else {
        NostrWatchSets::default()
    };
    let fire = match &nostr_watch {
        Some((_, true)) => {
            let w = watch_row.as_ref().ok_or_else(GateError::store)?;
            Some(state.privilege_for(w.id, || {
                let state = Arc::clone(state);
                let runtime = runtime.clone();
                PrivilegeFire::new(move |held: Vec<(NostrHeldTurn, CallerIdentity)>| {
                    let state = Arc::clone(&state);
                    let runtime = runtime.clone();
                    async move {
                        fire_held_turns(state, runtime, resolve_caller, held);
                    }
                })
            })?)
        }
        _ => None,
    };
    let watch_accept = watch_row.as_ref().map(|w| WatchAccept {
        policy_json: policy.as_str(),
        interval_secs: w.interval_secs as u64,
        allow: watch_sets.as_watch_allow(),
        owner: &watch_sets.owner,
        followees: &watch_sets.followees,
        privilege: fire.as_ref(),
    });
    let recorded_text = recorded_said_text(state, &row, said, &session_id);
    let prompt_suffix = if row.kind_id == "nostr" {
        nostr_prompt_suffix(&said.author_id, &said.text)
    } else {
        String::new()
    };
    let held = Cell::new(false);

    let accept = accept_inbound(
        &work,
        &row.owner_id,
        &agent_ids,
        &lookups,
        watch_accept,
        |_| {
            held.set(true);
            if record_inbound(&tx, &session_id, &row, said, &recorded_text).is_ok() {
                recorded.set(true);
            } else {
                record_failed.set(true);
            }
            NostrHeldTurn {
                session_id: session_id.clone(),
                instance_id: row.instance_id.clone(),
                agent_id: row.agent_id.clone(),
                binding_id: said.binding_id.clone(),
                origin: said.origin.clone(),
                author_id: said.author_id.clone(),
                text: said.text.clone(),
                images: said.attachments.clone(),
                address: row.address.clone(),
                owner_id: row.owner_id.clone(),
                kind_id: row.kind_id.clone(),
                delivery_mode: row.delivery_mode,
                prompt_suffix: prompt_suffix.clone(),
            }
        },
        |_, admitted| {
            if admitted.admitted_agent_ids.contains(&row.agent_id) {
                match record_inbound(&tx, &session_id, &row, said, &recorded_text) {
                    Ok(()) => recorded.set(true),
                    Err(_) => record_failed.set(true),
                }
            }
        },
        |_, admitted, _read| {
            if bundle_admit.is_some() {
                return;
            }
            if recorded.get() && admitted.admitted_agent_ids.contains(&row.agent_id) {
                run_after.set(true);
            }
        },
    );

    if record_failed.get() {
        let _ = tx.rollback();
        // 真因（session log insert の rusqlite エラー）は record_inbound 内の store_logged が
        // ERROR で出している。ここでは失敗地点カテゴリだけ detail に載せる。
        return Err(GateError::with_detail(
            ErrorCode::StoreError,
            "said.record_inbound",
        ));
    }

    match accept {
        Ok(()) => {}
        Err(_) => {
            return conclude_unrecorded_bundle(tx, &ctx, bundle_admit.as_ref());
        }
    }

    if !recorded.get() {
        return conclude_unrecorded_bundle(tx, &ctx, bundle_admit.as_ref());
    }

    let bundle_applied = match bundle_admit.as_ref() {
        Some(admit) => Some(apply_bundle_member(&tx, &said.binding_id, admit, true)?),
        None => None,
    };
    let seq = next_seq(&tx, &said.binding_id)?;
    tx.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES (?1, ?2, ?3)",
        params![said.binding_id, said.origin, seq],
    )
    .map_err(|e| GateError::store_logged("said.external_origins_insert", e))?;
    if let Some(applied) = bundle_applied {
        return finish_bundle(tx, &ctx, applied, Some(seq));
    }
    let enqueue = run_after.get() && !held.get();
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
            "extgate: session turn queue full; said rejected"
        );
        return Ok(SaidOutcome { seq: None });
    }
    tx.commit()
        .map_err(|e| GateError::store_logged("said.tx_commit", e))?;
    drop(conn);

    if row.kind_id == "nostr" {
        fire_nostr_relay(state, &row, said);
    }

    if enqueue {
        enqueue_turn(
            Arc::clone(state),
            runtime.clone(),
            resolve_caller,
            &row,
            said,
            &session_id,
            &prompt_suffix,
            Some(seq), // #933: この said の external_origins.seq（fold 済み集合との照合用）
            true,      // 単一メンション: 発端 said の origin へ返信
        );
    }

    Ok(SaidOutcome { seq: Some(seq) })
}
