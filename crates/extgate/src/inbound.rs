//! said → accept_inbound。V3 §6.2 / §7.2。

use std::cell::Cell;
#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    accept_inbound, delivery_effect, start_session_turn, AgentRuntime, CallerIdentity,
    InboundLookups, InboundWork, LiveInboundScope, NormalizedInbound, NormalizedInboundEvent,
    PrivilegeFire, RunRequest, TranscriptSource, WatchAccept,
};
use opencrab_db::queries::{
    get_agent_discord_config, get_agent_nostr_owner_pubkey, get_session_policy_json,
    get_session_watch, insert_session_log, is_trusted_user, SessionLogRow,
    TRUSTED_PLATFORM_EXTGATE, TRUSTED_PLATFORM_NOSTR,
};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

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
    let mut nostr_watch: Option<(i64, bool)> = None;
    if row.kind_id == "nostr" {
        match state.admit_nostr_said(&row.agent_id, &said.author_id, &said.text)? {
            NostrSaidDecision::Drop => {
                tx.commit().map_err(|_| GateError::store())?;
                return Ok(SaidOutcome { seq: None });
            }
            NostrSaidDecision::Accept { bundle: true, .. } => {
                let _ = tx.rollback();
                return Err(GateError::store());
            }
            NostrSaidDecision::Accept {
                watch_id,
                immediate,
                bundle: false,
            } => {
                nostr_watch = watch_id.map(|id| (id, immediate));
            }
        }
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
        Some((id, true)) => match get_session_watch(&tx, id) {
            Ok(Some(w)) if w.session_id == row.address => Some(w),
            Ok(_) | Err(_) => {
                let _ = tx.rollback();
                return Err(GateError::store());
            }
        },
        _ => None,
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
    let fire = match &watch_row {
        Some(w) => Some(state.privilege_for(w.id, || {
            let state = Arc::clone(state);
            let runtime = runtime.clone();
            PrivilegeFire::new(move |held: Vec<(NostrHeldTurn, CallerIdentity)>| {
                let state = Arc::clone(&state);
                let runtime = runtime.clone();
                async move {
                    fire_held_turns(state, runtime, resolve_caller, held);
                }
            })
        })?),
        None => None,
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
            tx.commit().map_err(|_| GateError::store())?;
            return Ok(SaidOutcome { seq: None });
        }
    }

    if !recorded.get() {
        tx.commit().map_err(|_| GateError::store())?;
        return Ok(SaidOutcome { seq: None });
    }

    let seq = next_seq(&tx, &said.binding_id)?;
    tx.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES (?1, ?2, ?3)",
        params![said.binding_id, said.origin, seq],
    )
    .map_err(|_| GateError::store())?;
    tx.commit().map_err(|_| GateError::store())?;
    drop(conn);

    if row.kind_id == "nostr" {
        fire_nostr_relay(state, &row, said);
    }

    if run_after.get() && !held.get() {
        enqueue_turn(
            Arc::clone(state),
            runtime.clone(),
            resolve_caller,
            &row,
            said,
            &session_id,
            &prompt_suffix,
        );
    }

    Ok(SaidOutcome { seq: Some(seq) })
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

fn enqueue_turn<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
    row: &OriginRow,
    said: &Said,
    session_id: &str,
    prompt_suffix: &str,
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
    locks.spawn_serialized(session_id.clone(), async move {
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
                start_session_turn(
                    &runtime,
                    TranscriptSource::External,
                    &inbound,
                    |raw| raw.to_string(),
                    |conversation| {
                        let mut req = RunRequest::new(
                            agent_id.clone(),
                            name.clone(),
                            session_id.clone(),
                            system.clone(),
                            conversation,
                            "extgate",
                            caller.clone(),
                        )
                        .with_image_urls(images.clone());
                        if kind_id == "nostr" {
                            req = req.with_live_inbound_scope(LiveInboundScope::OnlySpeaker(
                                author_id.clone(),
                            ));
                        }
                        req
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
                apply_delivery_effect(
                    &state,
                    &runtime,
                    &instance_id,
                    &binding_id,
                    &agent_id,
                    &session_id,
                    effect,
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
    );
}

fn fire_nostr_relay(state: &ExtgateState, row: &OriginRow, said: &Said) {
    let body = nostr_renderer_body(&said.text);
    let (_, label, _, _) = nostr_renderer_meta(&said.author_id, &said.text);
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

fn nostr_renderer_body(text: &str) -> &str {
    let Some(first) = text.lines().next() else {
        return text;
    };
    if first.starts_with(V1_PREFIX) {
        let rest = text.get(first.len()..).unwrap_or("");
        return rest.strip_prefix('\n').unwrap_or(rest);
    }
    text
}

fn nostr_prompt_suffix(author_id: &str, text: &str) -> String {
    let (kind, label, author_key, target) = nostr_renderer_meta(author_id, text);
    let author = nostr_author_label(author_id);
    format!(
        "[Nostr] {author} さんの投稿への応答です。\n\
         - 送信者: {author_key}（pubkey={pubkey}）\n\
         - 対象ノート: {target}\n\
         - 種別: kind:{kind}（{label}）\n\
         返信するなら nostr_reply(target=\"{target}\") を使ってください（target は返信先ノート）。\
         種別的に本文返信が不自然なもの（リアクション等）や、返信不要なら \
         NO_REPLY とだけ答えてください。",
        pubkey = author_id,
    )
}

fn nostr_author_label(author_id: &str) -> String {
    let short: String = author_id.chars().take(12).collect();
    format!("{short}…")
}

fn nostr_renderer_meta(author_id: &str, text: &str) -> (u32, &'static str, String, String) {
    let renderer = nostr_renderer_body(text);
    if let Some(meta) = parse_renderer_line(renderer) {
        return meta;
    }
    let (kind, event_id) =
        parse_v1_kind_and_event(text).expect("admitted nostr said has a V1 anchor");
    (
        kind,
        nostr_kind_label(kind),
        author_id.to_string(),
        event_id,
    )
}

fn parse_renderer_line(renderer: &str) -> Option<(u32, &'static str, String, String)> {
    let line = renderer.lines().last()?;
    let rest = line.strip_prefix("[Nostr kind:")?;
    let inner = rest.strip_suffix(']')?;
    let (kind_s, rest) = inner.split_once(' ')?;
    let kind: u32 = kind_s.parse().ok()?;
    let (label_from, target) = rest.split_once(" target=")?;
    let (label, author_key) = label_from.split_once(" from=")?;
    Some((
        kind,
        nostr_kind_from_label(label),
        author_key.to_string(),
        target.to_string(),
    ))
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
    fn prompt_suffix_is_verbatim() {
        let pk = "aa".repeat(32);
        let text = format!(
            "[NOSTRGATE/V1 {{\"kind\":1,\"event_id\":\"{}\"}}]\nhello\n[Nostr kind:1 リプライ from=npub1x target=note1abc]",
            "bb".repeat(32)
        );
        let suffix = nostr_prompt_suffix(&pk, &text);
        assert!(suffix.contains("nostr_reply(target=\"note1abc\")"));
        assert!(suffix.contains("kind:1（リプライ）"));
        assert!(suffix.contains("NO_REPLY"));
        assert!(suffix.contains("pubkey="));
        assert!(suffix.contains("npub1x"));
    }
}
