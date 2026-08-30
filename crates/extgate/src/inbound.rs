//! said → accept_inbound。V3 §6.2 / §7.2。

use std::cell::Cell;
#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    accept_inbound, delivery_effect, start_session_turn, AgentRuntime, CallerIdentity,
    InboundLookups, InboundWork, NormalizedInbound, NormalizedInboundEvent, PrivilegeFire,
    RunRequest, SubtaskCompletionSink, TranscriptSource, WatchAccept,
};
use opencrab_db::queries::{
    get_agent_discord_config, get_agent_nostr_owner_pubkey, get_session_policy_json,
    get_session_watch, insert_session_log, is_trusted_user, SessionLogRow,
    TRUSTED_PLATFORM_EXTGATE, TRUSTED_PLATFORM_NOSTR,
};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

use crate::bundle::{apply_bundle_member, BundleApply, NostrBundleAdmit};
use crate::completion::{v3_attach_dispatch, ExtgateCompletionSink};
use crate::delivery::apply_delivery_effect;
use crate::delivery_mode::{adjust_inbound_effect, DeliveryMode};
use crate::error::{ErrorCode, GateError};
use crate::ids::decode_config_b64;
use crate::listen::emit_activity;
use crate::protocol::Said;
use crate::registry::{ExtgateState, NostrHeldTurn, NostrSaidDecision, NostrWatchSets};
use crate::ResolveCallerFn;

pub struct SaidOutcome {
    pub seq: Option<i64>,
}

#[derive(Clone)]
struct OriginRow {
    instance_id: String,
    kind_id: String,
    address: String,
    agent_id: String,
    owner_id: String,
    delivery_mode: DeliveryMode,
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

    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;

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
            Ok(None) | Err(_) => {
                let _ = tx.rollback();
                return Err(GateError::store());
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
        kind_label: "said",
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
        return Err(GateError::store());
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
    .map_err(|_| GateError::store())?;
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
    tx.commit().map_err(|_| GateError::store())?;
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
            true, // 単一メンション: 発端 said の origin へ返信
        );
    }

    Ok(SaidOutcome { seq: Some(seq) })
}

struct BundleCtx<'a, R> {
    state: &'a Arc<ExtgateState>,
    runtime: &'a R,
    resolve_caller: ResolveCallerFn,
    row: &'a OriginRow,
    said: &'a Said,
    session_id: &'a str,
}

fn conclude_unrecorded_bundle<R: AgentRuntime>(
    tx: Transaction<'_>,
    ctx: &BundleCtx<'_, R>,
    bundle: Option<&NostrBundleAdmit>,
) -> Result<SaidOutcome, GateError> {
    let Some(admit) = bundle else {
        tx.commit().map_err(|_| GateError::store())?;
        return Ok(SaidOutcome { seq: None });
    };
    let applied = apply_bundle_member(&tx, &ctx.said.binding_id, admit, false)?;
    finish_bundle(tx, ctx, applied, None)
}

fn finish_bundle<R: AgentRuntime>(
    tx: Transaction<'_>,
    ctx: &BundleCtx<'_, R>,
    applied: BundleApply,
    seq: Option<i64>,
) -> Result<SaidOutcome, GateError> {
    let pending = if applied.enqueue {
        let origin = applied
            .trigger_origin
            .as_deref()
            .ok_or_else(GateError::store)?;
        let trigger = load_bundle_trigger(&tx, ctx.session_id, origin)?;
        Some((trigger, origin.to_string(), applied.new_admitted))
    } else {
        None
    };
    let drop_turn = pending.is_some() && !ctx.state.turn_queues.has_room(ctx.session_id);
    tx.commit().map_err(|_| GateError::store())?;
    if seq.is_some() {
        fire_nostr_relay(ctx.state, ctx.row, ctx.said);
    }
    if drop_turn {
        let dropped = ctx.state.turn_queues.note_dropped();
        #[cfg(any(test, feature = "extgate-probe"))]
        ctx.state
            .probe
            .turn_queue_dropped
            .fetch_add(1, Ordering::SeqCst);
        tracing::warn!(
            session_id = ctx.session_id,
            dropped_total = dropped,
            "extgate: session turn queue full; bundle turn dropped"
        );
        return Ok(SaidOutcome { seq });
    }
    if let Some((trigger, origin, n)) = pending {
        let suffix = bundle_prompt_suffix(n);
        let trigger_said = Said {
            id: String::new(),
            binding_id: ctx.said.binding_id.clone(),
            origin,
            author_id: trigger.0,
            text: trigger.1,
            attachments: trigger.2,
        };
        enqueue_turn(
            Arc::clone(ctx.state),
            ctx.runtime.clone(),
            ctx.resolve_caller,
            ctx.row,
            &trigger_said,
            ctx.session_id,
            &suffix,
            false, // bundle: 単一返信先が無い（gateway drop・エアリプは nostr_post）
        );
    }
    Ok(SaidOutcome { seq })
}

fn load_bundle_trigger(
    tx: &Transaction<'_>,
    session_id: &str,
    origin: &str,
) -> Result<(String, String, Vec<String>), GateError> {
    match tx.query_row(
        "SELECT speaker_id, content, metadata_json FROM memory_sessions
         WHERE session_id = ?1 AND json_extract(metadata_json, '$.external_origin') = ?2
         ORDER BY id DESC LIMIT 1",
        params![session_id, origin],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    ) {
        Ok((speaker, content, meta)) => {
            let images = meta
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| v.get("image_urls").cloned())
                .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
                .unwrap_or_default();
            Ok((speaker.unwrap_or_default(), content, images))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) | Err(_) => Err(GateError::store()),
    }
}

fn bundle_prompt_suffix(count: u32) -> String {
    format!(
        "[Nostr] タイムライン watch の束ね（{count} 件）です。窓内を 1 ターンの文脈に載せています。\
         心が動いた投稿にはエアリプ（nostr_post による独立投稿）で触れてよい（特定ノートへの返信には\
         なりません）。反応不要なら NO_REPLY とだけ答えてください。"
    )
}

fn binding_said_error(
    state: &ExtgateState,
    instance_id: &str,
    binding_id: &str,
) -> Result<SaidOutcome, GateError> {
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    match binding_status(&conn, instance_id, binding_id)? {
        BindingStatus::Absent => Err(GateError::new(ErrorCode::BindingUnknown)),
        BindingStatus::Closed => Err(GateError::new(ErrorCode::BindingClosed)),
        BindingStatus::OtherInstance | BindingStatus::Open => {
            Err(GateError::new(ErrorCode::InstanceNotReady))
        }
    }
}

enum BindingStatus {
    Absent,
    Closed,
    OtherInstance,
    Open,
}

fn binding_status(
    conn: &Connection,
    instance_id: &str,
    binding_id: &str,
) -> Result<BindingStatus, GateError> {
    let row = conn.query_row(
        "SELECT instance_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
        params![binding_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
    );
    match row {
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(BindingStatus::Absent),
        Err(_) => Err(GateError::store()),
        Ok((inst, closed)) => {
            if inst != instance_id {
                Ok(BindingStatus::OtherInstance)
            } else if closed.is_some() {
                Ok(BindingStatus::Closed)
            } else {
                Ok(BindingStatus::Open)
            }
        }
    }
}

fn load_origin_row(
    tx: &Transaction<'_>,
    instance_id: &str,
    binding_id: &str,
) -> Result<Option<OriginRow>, GateError> {
    let result = tx.query_row(
        "SELECT b.instance_id, i.kind_id, b.address, a.agent_id, b.closed_at, i.config_b64
         FROM gate_bindings b
         JOIN gate_instances i ON i.instance_id = b.instance_id
         JOIN agents a ON a.subject_id = i.subject_id
         WHERE b.binding_id = ?1 AND i.deleted_at IS NULL",
        params![binding_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, String>(5)?,
            ))
        },
    );
    match result {
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(GateError::store()),
        Ok((inst, kind_id, address, agent_id, closed, config_b64)) => {
            if inst != instance_id || closed.is_some() {
                return Ok(None);
            }
            let owner_id = if kind_id == "nostr" {
                get_agent_nostr_owner_pubkey(tx, &agent_id).map_err(|_| GateError::store())?
            } else {
                get_agent_discord_config(tx, &agent_id)
                    .ok()
                    .flatten()
                    .map(|c| c.owner_discord_id)
                    .unwrap_or_default()
            };
            let config_bytes = decode_config_b64(&config_b64)?;
            let delivery_mode =
                crate::delivery_mode::delivery_mode_from_config_bytes(&config_bytes)
                    .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
            Ok(Some(OriginRow {
                instance_id: inst,
                kind_id,
                address,
                agent_id,
                owner_id,
                delivery_mode,
            }))
        }
    }
}

fn existing_seq(
    tx: &Transaction<'_>,
    binding_id: &str,
    origin: &str,
) -> Result<Option<i64>, GateError> {
    match tx.query_row(
        "SELECT seq FROM external_origins WHERE binding_id = ?1 AND origin = ?2",
        params![binding_id, origin],
        |r| r.get(0),
    ) {
        Ok(seq) => Ok(Some(seq)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(GateError::store()),
    }
}

fn next_seq(tx: &Transaction<'_>, binding_id: &str) -> Result<i64, GateError> {
    tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM external_origins WHERE binding_id = ?1",
        params![binding_id],
        |r| r.get(0),
    )
    .map_err(|_| GateError::store())
}

fn record_inbound(
    tx: &Transaction<'_>,
    session_id: &str,
    row: &OriginRow,
    said: &Said,
    content: &str,
) -> Result<(), GateError> {
    let mut meta = serde_json::json!({
        "source": TranscriptSource::External.inbound(),
        "user_name": "",
        "channel_id": row.address,
    });
    if !said.attachments.is_empty() {
        meta["image_urls"] = serde_json::json!(said.attachments);
    }
    if row.kind_id == "nostr" {
        meta["external_origin"] = serde_json::json!(said.origin);
    }
    insert_session_log(
        tx,
        &SessionLogRow {
            id: None,
            agent_id: row.agent_id.clone(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(said.author_id.clone()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        },
    )
    .map_err(|_| GateError::store())?;
    Ok(())
}

/// owner 一致または trusted_users。query failure は false。
pub fn dm_allowed(conn: &Connection, sender: &str, agent_id: &str, owner_id: &str) -> bool {
    if opencrab_core::owner::is_owner_id(owner_id, sender) {
        return true;
    }
    is_trusted_user(conn, TRUSTED_PLATFORM_EXTGATE, sender, agent_id)
}

/// 当該 agent/instance/address の open binding exact 1 行だけ true。
pub fn channel_whitelisted(
    conn: &Connection,
    agent_id: &str,
    instance_id: &str,
    address: &str,
) -> bool {
    let result = conn.query_row(
        "SELECT COUNT(*) FROM gate_bindings b
         JOIN gate_instances i ON i.instance_id = b.instance_id
         JOIN agents a ON a.subject_id = i.subject_id
         WHERE a.agent_id = ?1 AND b.instance_id = ?2 AND b.address = ?3
           AND b.closed_at IS NULL AND i.deleted_at IS NULL",
        params![agent_id, instance_id, address],
        |r| r.get::<_, i64>(0),
    );
    matches!(result, Ok(1))
}

#[allow(clippy::too_many_arguments)]
fn enqueue_turn<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
    row: &OriginRow,
    said: &Said,
    session_id: &str,
    prompt_suffix: &str,
    // 単一メンション turn は発端 said の origin へ返信（say payload の reply_target に載せる）。
    // bundle turn は単一返信先が無いので false（gateway は drop・エアリプは nostr_post）。
    // gateway 側の pending_turn 相関は activity ended が say より先に届き消えるため当てにしない。
    reply_to_origin: bool,
) {
    let locks = runtime.session_locks();
    let session_id = session_id.to_string();
    let agent_id = row.agent_id.clone();
    let instance_id = row.instance_id.clone();
    let binding_id = said.binding_id.clone();
    let author_id = said.author_id.clone();
    let text = said.text.clone();
    let images = said.attachments.clone();
    let address = row.address.clone();
    let origin = said.origin.clone();
    let owner_id = row.owner_id.clone();
    let kind_id = row.kind_id.clone();
    let delivery_mode = row.delivery_mode;
    let prompt_suffix = prompt_suffix.to_string();
    // say の返信先（発端イベント origin）。単一メンションのみ。bundle は None。
    let reply_target: Option<String> = if reply_to_origin {
        Some(origin.clone())
    } else {
        None
    };
    if !state.turn_queues.try_reserve(&session_id) {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .turn_queue_dropped
            .fetch_add(1, Ordering::SeqCst);
        return;
    }
    let queues = Arc::clone(&state.turn_queues);
    let session_key = session_id.clone();
    queues.submit(&session_key, async move {
        let lock_id = session_id.clone();
        locks
            .run_serialized(&lock_id, async move {
                #[cfg(any(test, feature = "extgate-probe"))]
                state
                    .probe
                    .start_session_turn_count
                    .fetch_add(1, Ordering::SeqCst);
                let activity_id = uuid::Uuid::new_v4().to_string();
                emit_activity(&state, &instance_id, &binding_id, &activity_id, "started").await;
                let caller = match state.db.lock() {
                    Ok(conn) => resolve_caller(
                        &conn,
                        if kind_id == "nostr" {
                            TRUSTED_PLATFORM_NOSTR
                        } else {
                            TRUSTED_PLATFORM_EXTGATE
                        },
                        &[author_id.as_str()],
                        &agent_id,
                        &owner_id,
                    ),
                    Err(_) => CallerIdentity::Agent,
                };
                let (system, name) = runtime.build_agent_context(&agent_id, &caller);
                let system = if prompt_suffix.is_empty() {
                    system
                } else {
                    format!("{system}\n\n{prompt_suffix}")
                };
                let turn_res = {
                    let runtime = runtime.clone();
                    let session_id = session_id.clone();
                    let agent_id = agent_id.clone();
                    let author_id = author_id.clone();
                    let address = address.clone();
                    let text = text.clone();
                    let images = images.clone();
                    let origin = origin.clone();
                    let kind_id = kind_id.clone();
                    let state = Arc::clone(&state);
                    let instance_id = instance_id.clone();
                    let binding_id = binding_id.clone();
                    let prompt_suffix = prompt_suffix.clone();
                    tokio::spawn(async move {
                        let inbound = NormalizedInbound {
                            session_id: &session_id,
                            agent_id: &agent_id,
                            sender_id: &author_id,
                            sender_name: "",
                            avatar_url: None,
                            channel_id: Some(&address),
                            pubkey: if kind_id == "nostr" {
                                Some(author_id.as_str())
                            } else {
                                None
                            },
                            text: &text,
                            image_urls: &images,
                            external_id: &origin,
                        };
                        let registry = runtime.subtask_registry_for(&session_id);
                        let sink: Arc<dyn SubtaskCompletionSink> =
                            Arc::new(ExtgateCompletionSink {
                                state,
                                runtime: runtime.clone(),
                                instance_id,
                                binding_id,
                                agent_id: agent_id.clone(),
                                session_id: session_id.clone(),
                                kind_id: kind_id.clone(),
                                author_id: author_id.clone(),
                                delivery_mode,
                                prompt_suffix,
                            });
                        start_session_turn(
                            &runtime,
                            TranscriptSource::External,
                            &inbound,
                            &system,
                            // extgate は会話へ runtime context を前置しない（wrap は素通し）。
                            // 予算計上もそれに合わせて空文字（実 request と一致させる契約）。
                            "",
                            |raw| raw.to_string(),
                            |conversation| {
                                v3_attach_dispatch(
                                    RunRequest::new(
                                        agent_id.clone(),
                                        name.clone(),
                                        session_id.clone(),
                                        system.clone(),
                                        conversation,
                                        "extgate",
                                        caller.clone(),
                                    )
                                    .with_image_urls(images.clone())
                                    // 発端イベントの origin を subtask へ引き継ぐ。subtask 完了時の
                                    // resume ターンの say がこの origin へ返信できるようにする
                                    // （settlement→SubtaskSettled.reply_target 経由）。
                                    .with_reply_target(origin.clone()),
                                    &kind_id,
                                    author_id.clone(),
                                    registry.clone(),
                                    Arc::clone(&sink),
                                )
                            },
                        )
                        .await
                    })
                    .await
                };
                emit_activity(&state, &instance_id, &binding_id, &activity_id, "ended").await;
                match turn_res {
                    Ok(turn) => {
                        let effect = match turn {
                            Some(r) => delivery_effect(r),
                            None => opencrab_actions::DeliveryEffect::Empty,
                        };
                        let effect = adjust_inbound_effect(delivery_mode, effect);
                        // 単一メンションは発端 origin を say payload に明示（gateway が e-tag reply）。
                        // bundle は None（gateway drop・エアリプは nostr_post）。gateway の
                        // pending_turn 相関は activity ended が say より先に届き消えるため使わない。
                        apply_delivery_effect(
                            &state,
                            &runtime,
                            &instance_id,
                            &binding_id,
                            &agent_id,
                            &session_id,
                            effect,
                            reply_target.as_deref(),
                        )
                        .await;
                    }
                    Err(_) => {
                        tracing::error!("extgate turn task panicked");
                        crate::close::close_live(
                            &state,
                            Some(&instance_id),
                            None,
                            ErrorCode::Disconnect,
                            None,
                            None,
                        )
                        .await;
                        state.halt();
                    }
                }
            })
            .await;
    });
}

fn fire_held_turns<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
    held: Vec<(NostrHeldTurn, CallerIdentity)>,
) {
    let Some((last, _)) = held.last() else {
        return;
    };
    let said = Said {
        id: String::new(),
        binding_id: last.binding_id.clone(),
        origin: last.origin.clone(),
        author_id: last.author_id.clone(),
        text: last.text.clone(),
        attachments: last.images.clone(),
    };
    let row = OriginRow {
        instance_id: last.instance_id.clone(),
        kind_id: last.kind_id.clone(),
        address: last.address.clone(),
        agent_id: last.agent_id.clone(),
        owner_id: last.owner_id.clone(),
        delivery_mode: last.delivery_mode,
    };
    enqueue_turn(
        state,
        runtime,
        resolve_caller,
        &row,
        &said,
        &last.session_id,
        &last.prompt_suffix,
        true, // 保留していた単一メンション: 発端 said の origin へ返信
    );
}

fn fire_nostr_relay(state: &ExtgateState, row: &OriginRow, said: &Said) {
    let body = nostr_renderer_body(&said.text);
    let (_, label) = nostr_renderer_meta(&said.author_id, &said.text);
    let author = nostr_author_label(&said.author_id);
    let text = format!("[Nostr / {label}] {author}\n{body}");
    state.relay_nostr_inbound(&row.agent_id, text);
}

fn recorded_said_text(
    state: &ExtgateState,
    row: &OriginRow,
    said: &Said,
    session_id: &str,
) -> String {
    if row.kind_id != "nostr" {
        return said.text.clone();
    }
    let renderer = nostr_renderer_body(&said.text);
    opencrab_actions::sanitize_tool_result_for_log(
        "nostr_inbound",
        renderer,
        session_id,
        &said.origin,
        state.nostr_workspace_root(&row.agent_id).as_deref(),
    )
}

const V1_PREFIX: &str = "[NOSTRGATE/V1 ";

const BUNDLE_PREFIX: &str = "[NOSTRBUNDLE/V1 ";

fn nostr_renderer_body(text: &str) -> &str {
    let Some(first) = text.lines().next() else {
        return text;
    };
    if !first.starts_with(V1_PREFIX) {
        return text;
    }
    let rest = text.get(first.len()..).unwrap_or("");
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let Some(second) = rest.lines().next() else {
        return rest;
    };
    if second.starts_with(BUNDLE_PREFIX) {
        let after = rest.get(second.len()..).unwrap_or("");
        return after.strip_prefix('\n').unwrap_or(after);
    }
    rest
}

fn nostr_prompt_suffix(author_id: &str, text: &str) -> String {
    // §9A / row296: pubkey= と対象ノートの生 ID をプロンプトから排除し短縮する。
    // 返信は発端投稿へ自動配送されるので LLM は対象 ID を指定しない。
    let (kind, label) = nostr_renderer_meta(author_id, text);
    let author = nostr_author_label(author_id);
    format!(
        "[Nostr] {author} さんの投稿への応答です。\n\
         - 種別: kind:{kind}（{label}）\n\
         返信する内容をそのまま本文で書いてください（あなたの応答はこの投稿への返信として\
         自動で投稿されます）。種別的に本文返信が不自然なもの（リアクション等）や、返信不要なら \
         NO_REPLY とだけ答えてください。",
    )
}

fn nostr_author_label(author_id: &str) -> String {
    let short: String = author_id.chars().take(12).collect();
    format!("{short}…")
}

fn nostr_renderer_meta(_author_id: &str, text: &str) -> (u32, &'static str) {
    let renderer = nostr_renderer_body(text);
    if let Some(meta) = parse_renderer_line(renderer) {
        return meta;
    }
    let (kind, _event_id) =
        parse_v1_kind_and_event(text).expect("admitted nostr said has a V1 anchor");
    (kind, nostr_kind_label(kind))
}

/// history 行 `[Nostr kind:{kind} {label}]` から種別だけを取る（§9A.2 で from=/target= は撤去）。
fn parse_renderer_line(renderer: &str) -> Option<(u32, &'static str)> {
    let line = renderer.lines().last()?;
    let rest = line.strip_prefix("[Nostr kind:")?;
    let inner = rest.strip_suffix(']')?;
    let (kind_s, label) = inner.split_once(' ')?;
    let kind: u32 = kind_s.parse().ok()?;
    Some((kind, nostr_kind_from_label(label)))
}

fn parse_v1_kind_and_event(text: &str) -> Option<(u32, String)> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix(V1_PREFIX)?;
    let json = rest.strip_suffix(']')?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let kind = value.get("kind")?.as_u64()? as u32;
    let event_id = value.get("event_id")?.as_str()?.to_string();
    Some((kind, event_id))
}

fn nostr_kind_from_label(label: &str) -> &'static str {
    match label {
        "DM" => "DM",
        "リアクション" => "リアクション",
        "長文" => "長文",
        "リプライ" => "リプライ",
        "リポスト" => "リポスト",
        _ => "メンション",
    }
}

fn nostr_kind_label(kind: u32) -> &'static str {
    match kind {
        4 | 1059 => "DM",
        7 => "リアクション",
        6 | 16 => "リポスト",
        30023 => "長文",
        _ => "メンション",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_body_strips_v1_line() {
        let text =
            "[NOSTRGATE/V1 {\"kind\":1}]\nhello\n[Nostr kind:1 リプライ from=aa target=note1x]";
        assert_eq!(
            nostr_renderer_body(text),
            "hello\n[Nostr kind:1 リプライ from=aa target=note1x]"
        );
    }

    #[test]
    fn renderer_body_strips_bundle_members_line() {
        let text = "[NOSTRGATE/V1 {\"kind\":1}]\n[NOSTRBUNDLE/V1 [\"o1\"]]\nhello";
        assert_eq!(nostr_renderer_body(text), "hello");
    }

    #[test]
    fn prompt_suffix_omits_raw_identifiers() {
        let pk = "aa".repeat(32);
        let text = format!(
            "[NOSTRGATE/V1 {{\"kind\":1,\"event_id\":\"{}\"}}]\nhello\n[Nostr kind:1 リプライ]",
            "bb".repeat(32)
        );
        let suffix = nostr_prompt_suffix(&pk, &text);
        // nostr_reply 露出撤去（返信は say 一本 / #840）: プロンプトに nostr_reply を出さない。
        assert!(!suffix.contains("nostr_reply"));
        // §9A / row296: 生 ID（対象ノート note1・pubkey hex）をプロンプトから排除する。
        assert!(!suffix.contains("note1"));
        assert!(!suffix.contains("pubkey="));
        assert!(!suffix.contains(&pk));
        assert!(!suffix.contains("対象ノート"));
        // 返信は本文をそのまま書かせる say ベースの文言になっている。
        assert!(suffix.contains("そのまま本文で書いて"));
        assert!(suffix.contains("kind:1（リプライ）"));
        assert!(suffix.contains("NO_REPLY"));
    }
}
