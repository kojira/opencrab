//! said → accept_inbound。V3 §6.2 / §7.2。

use std::cell::Cell;
#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    accept_inbound, delivery_effect, start_session_turn, AgentRuntime, CallerIdentity,
    InboundLookups, InboundWork, NormalizedInbound, NormalizedInboundEvent, RunRequest,
    TranscriptSource,
};
use opencrab_db::queries::{
    get_agent_discord_config, insert_session_log, is_trusted_user, SessionLogRow,
    TRUSTED_PLATFORM_EXTGATE,
};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

use crate::delivery::apply_delivery_effect;
use crate::delivery_mode::{adjust_inbound_effect, DeliveryMode};
use crate::listen::emit_activity;
use crate::error::{ErrorCode, GateError};
use crate::ids::decode_config_b64;
use crate::protocol::Said;
use crate::registry::ExtgateState;
use crate::ResolveCallerFn;

pub struct SaidOutcome {
    pub seq: Option<i64>,
}

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
    let session_id = match opencrab_db::queries::canonical_session_id(
        &tx,
        &said.binding_id,
        &row.address,
    ) {
        Ok(Some(id)) => id,
        Ok(None) | Err(_) => {
            let _ = tx.rollback();
            return Err(GateError::store());
        }
    };
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
        resolve_caller(&tx, TRUSTED_PLATFORM_EXTGATE, &[sender], agent_id, owner)
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

    let accept = accept_inbound(
        &work,
        &row.owner_id,
        &agent_ids,
        &lookups,
        None,
        |_| (),
        |_, admitted| {
            if admitted.admitted_agent_ids.contains(&row.agent_id) {
                match record_inbound(&tx, &session_id, &row, said) {
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

    if run_after.get() {
        enqueue_turn(
            Arc::clone(state),
            runtime.clone(),
            resolve_caller,
            &row,
            said,
            &session_id,
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
            let owner_id = get_agent_discord_config(tx, &agent_id)
                .ok()
                .flatten()
                .map(|c| c.owner_discord_id)
                .unwrap_or_default();
            let config_bytes = decode_config_b64(&config_b64)?;
            let delivery_mode = crate::delivery_mode::delivery_mode_from_config_bytes(&config_bytes)
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
            content: said.text.clone(),
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
    let delivery_mode = row.delivery_mode;
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
                TRUSTED_PLATFORM_EXTGATE,
                &[author_id.as_str()],
                &agent_id,
                &owner_id,
            ),
            Err(_) => CallerIdentity::Agent,
        };
        let (system, name) = runtime.build_agent_context(&agent_id, &caller);
        let turn_res = {
            let runtime = runtime.clone();
            let session_id = session_id.clone();
            let agent_id = agent_id.clone();
            let author_id = author_id.clone();
            let address = address.clone();
            let text = text.clone();
            let images = images.clone();
            let origin = origin.clone();
            tokio::spawn(async move {
                let inbound = NormalizedInbound {
                    session_id: &session_id,
                    agent_id: &agent_id,
                    sender_id: &author_id,
                    sender_name: "",
                    avatar_url: None,
                    channel_id: Some(&address),
                    pubkey: None,
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
