//! DeliveryEffect → reply + sending 同一 TX、その後 say 1 回。V3 §6.3 / §7.4。

#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{DeliveryEffect, TranscriptSource};
use opencrab_db::queries::{insert_session_log, SessionLogRow};
use rusqlite::{params, TransactionBehavior};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::listen::emit_turn_failed;
use crate::operations::DeliveryGuarantee;
use crate::protocol::{say_frame, write_json};
use crate::registry::{ExtgateState, Pending};

#[allow(clippy::too_many_arguments)]
pub async fn apply_delivery_effect(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    agent_id: &str,
    session_id: &str,
    effect: DeliveryEffect,
    reply_target: Option<&str>,
) -> Option<String> {
    match effect {
        DeliveryEffect::Text { body, .. } => {
            if body.is_empty() {
                tracing::error!("DeliveryEffect::Text body is empty; fail-loud");
                crate::close::close_live(
                    state,
                    Some(instance_id),
                    None,
                    ErrorCode::Disconnect,
                    None,
                    None,
                )
                .await;
                state.halt();
                return None;
            }
            match send_text(
                state,
                instance_id,
                binding_id,
                agent_id,
                session_id,
                &body,
                reply_target,
            )
            .await
            {
                Ok(delivery_id) => return Some(delivery_id),
                Err(e) => {
                    if e.code == ErrorCode::NotConnected || e.code == ErrorCode::BindingClosed {
                        return None;
                    }
                    tracing::error!(code = e.code.as_str(), "delivery failed");
                    if e.code == ErrorCode::StoreError {
                        crate::close::close_live(
                            state,
                            Some(instance_id),
                            None,
                            ErrorCode::StoreError,
                            None,
                            None,
                        )
                        .await;
                        state.halt();
                    }
                }
            }
        }
        DeliveryEffect::NoReply => {
            // 配送層は既に visible_speech_after_markers で沈黙判定済み。ここは何もしない。
        }
        DeliveryEffect::Empty | DeliveryEffect::Failed { .. } => {
            if let DeliveryEffect::Failed { error } = &effect {
                tracing::error!(error = %error, "session turn failed");
                // R3(❌): ターン失敗を発端 origin つきで gateway へ通知する（gateway が ❌ を付ける）。
                // error 本文は wire に載せない（多エージェント相互反応ループ防止・#668）。単一メンション
                // のみ reply_target=Some（bundle/曖昧は None＝付ける先が無いので通知しない）。
                if let Some(origin) = reply_target {
                    emit_turn_failed(state, instance_id, binding_id, origin).await;
                }
            }
        }
    }
    None
}

/// 明示終了前の途中iterationの発話を、最終応答と同じ経路
/// （[`send_text`] = say配送＋memory_sessions speech保存）で1件配送・保存する。
/// engineがループ中にawaitし、`Err`はターンを失敗させる。
/// 呼び出し側（extgate inbound）が `delivery_mode` で say 抑止（ToolDriven）を判断してから呼ぶ。
#[allow(clippy::too_many_arguments)]
pub async fn deliver_intermediate_say(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    agent_id: &str,
    session_id: &str,
    body: &str,
    reply_target: Option<&str>,
) -> Result<String, GateError> {
    send_text(
        state,
        instance_id,
        binding_id,
        agent_id,
        session_id,
        body,
        reply_target,
    )
    .await
}

async fn send_text(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    agent_id: &str,
    session_id: &str,
    body: &str,
    reply_target: Option<&str>,
) -> Result<String, GateError> {
    let (writer, delivery_guarantee, adapter_protocol_digest) = {
        let reg = state.lock_registry()?;
        let live = reg
            .get(instance_id)
            .ok_or_else(|| GateError::new(ErrorCode::NotConnected))?;
        if !live.acknowledged.contains(binding_id) {
            return Err(GateError::new(ErrorCode::NotConnected));
        }
        (
            live.writer.clone(),
            live.delivery_guarantee,
            live.declaration_digest.clone(),
        )
    };

    let delivery_id = Uuid::new_v4().to_string();
    let now = now_nanos();
    let mut payload_value = serde_json::json!({"text": body});
    if let Some(target) = reply_target {
        payload_value["reply_target"] = serde_json::json!(target);
    }
    let payload = payload_value.to_string();
    let payload_digest = Sha256::digest(payload.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let prepared_frame = say_frame(
        &delivery_id,
        binding_id,
        body,
        reply_target,
        &payload_digest,
        delivery_guarantee,
        &adapter_protocol_digest,
    );
    let prepared_frame_json = prepared_frame.to_string();
    {
        let mut conn = state.db.lock().map_err(|_| GateError::store())?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| GateError::store())?;
        let open = tx.query_row(
            "SELECT instance_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
            params![binding_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        );
        match open {
            Ok((inst, None)) if inst == instance_id => {}
            Ok(_) => {
                let _ = tx.rollback();
                return Err(GateError::new(ErrorCode::BindingClosed));
            }
            Err(_) => {
                let _ = tx.rollback();
                return Err(GateError::store());
            }
        }
        #[cfg(any(test, feature = "extgate-probe"))]
        if state.probe.fail_reply_log.load(Ordering::SeqCst) {
            let _ = tx.rollback();
            return Err(GateError::store());
        }
        insert_session_log(
            &tx,
            &SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: body.to_string(),
                speaker_id: Some(agent_id.to_string()),
                turn_number: None,
                metadata_json: Some(
                    serde_json::json!({"source": TranscriptSource::new("external", "external_response").reply()}).to_string(),
                ),
                created_at: None,
            },
        )
        .map_err(|_| GateError::store())?;
        #[cfg(any(test, feature = "extgate-probe"))]
        if state.probe.fail_delivery_insert.load(Ordering::SeqCst) {
            let _ = tx.rollback();
            return Err(GateError::store());
        }
        tx.execute(
            "INSERT INTO deliveries
             (delivery_id, binding_id, payload_json, state, error, created_at, updated_at,
              payload_digest, delivery_guarantee, prepared_protocol_digest, frame_kind,
              prepared_frame_json)
             VALUES (?1, ?2, ?3, 'sending', NULL, ?4, ?4, ?5, ?6, ?7, 'say', ?8)",
            params![
                delivery_id,
                binding_id,
                payload,
                now,
                payload_digest,
                delivery_guarantee.as_str(),
                adapter_protocol_digest,
                prepared_frame_json,
            ],
        )
        .map_err(|_| GateError::store())?;
        tx.commit().map_err(|_| GateError::store())?;
    }

    {
        let mut reg = state.lock_registry()?;
        let Some(live) = reg.get_mut(instance_id) else {
            mark_indeterminate(state, std::slice::from_ref(&delivery_id))?;
            return Err(GateError::new(ErrorCode::Disconnect));
        };
        live.pending.insert(
            delivery_id.clone(),
            Pending::Say {
                delivery_id: delivery_id.clone(),
            },
        );
    }

    let write_err = write_json(&writer, &prepared_frame).await.is_err();
    #[cfg(any(test, feature = "extgate-probe"))]
    let write_err = write_err || state.probe.fail_say_write.load(Ordering::SeqCst);
    if write_err {
        mark_indeterminate(state, std::slice::from_ref(&delivery_id))?;
        crate::close::close_live(
            state,
            Some(instance_id),
            None,
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(GateError::new(ErrorCode::Disconnect));
    }
    Ok(delivery_id)
}

pub(crate) async fn replay_pending_for_binding(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
) -> Result<(), GateError> {
    let (writer, live_guarantee) = {
        let reg = state.lock_registry()?;
        let live = reg
            .get(instance_id)
            .ok_or_else(|| GateError::new(ErrorCode::NotConnected))?;
        (live.writer.clone(), live.delivery_guarantee)
    };
    let rows = {
        let conn = state.db.lock().map_err(|_| GateError::store())?;
        let mut stmt = conn
            .prepare(
                "SELECT delivery_id,payload_json,payload_digest,delivery_guarantee,
                        prepared_protocol_digest,frame_kind,prepared_frame_json
                 FROM deliveries WHERE binding_id=?1 AND state='sending'
                 ORDER BY created_at,delivery_id",
            )
            .map_err(|_| GateError::store())?;
        let rows = stmt
            .query_map([binding_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|_| GateError::store())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| GateError::store())?;
        rows
    };
    for (
        delivery_id,
        payload_json,
        payload_digest,
        guarantee,
        protocol_digest,
        frame_kind,
        prepared_frame_json,
    ) in rows
    {
        let Some(required) = DeliveryGuarantee::parse(&guarantee) else {
            continue;
        };
        if !live_guarantee.satisfies(required) {
            continue;
        }
        let payload: serde_json::Value =
            serde_json::from_str(&payload_json).map_err(|_| GateError::store())?;
        let _payload_digest = payload_digest.ok_or_else(GateError::store)?;
        let _protocol_digest = protocol_digest.ok_or_else(GateError::store)?;
        let prepared_frame: serde_json::Value = serde_json::from_str(
            prepared_frame_json
                .as_deref()
                .ok_or_else(GateError::store)?,
        )
        .map_err(|_| GateError::store())?;
        let pending_id = prepared_frame
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(GateError::store)?
            .to_string();
        let pending = match frame_kind.as_str() {
            "say" => Pending::Say {
                delivery_id: delivery_id.clone(),
            },
            "invoke" => Pending::Utterance {
                delivery_id: delivery_id.clone(),
                binding_id: binding_id.to_string(),
                reply_target: payload
                    .get("reply_target")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            },
            _ => return Err(GateError::store()),
        };
        {
            let mut reg = state.lock_registry()?;
            let live = reg
                .get_mut(instance_id)
                .ok_or_else(|| GateError::new(ErrorCode::NotConnected))?;
            live.pending.insert(pending_id, pending);
        }
        if write_json(&writer, &prepared_frame).await.is_err() {
            return Err(GateError::new(ErrorCode::Disconnect));
        }
    }
    Ok(())
}

pub fn mark_indeterminate(state: &ExtgateState, delivery_ids: &[String]) -> Result<(), GateError> {
    if delivery_ids.is_empty() {
        return Ok(());
    }
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|_| GateError::store())?;
    let now = now_nanos();
    for id in delivery_ids {
        tx.execute(
            "UPDATE deliveries
             SET state = 'indeterminate', error = 'disconnect', updated_at = ?2,
             acknowledged_at = ?2
             WHERE delivery_id = ?1 AND state = 'sending'",
            params![id, now],
        )
        .map_err(|_| GateError::store())?;
    }
    tx.commit().map_err(|_| GateError::store())?;
    Ok(())
}

pub fn mark_delivered(state: &ExtgateState, delivery_id: &str) -> Result<(), GateError> {
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    let now = now_nanos();
    conn.execute(
        "UPDATE deliveries
         SET state = 'delivered', error = NULL, updated_at = ?2,
             acknowledged_at = ?2
         WHERE delivery_id = ?1 AND state = 'sending'",
        params![delivery_id, now],
    )
    .map_err(|_| GateError::store())?;
    Ok(())
}

pub fn mark_failed(state: &ExtgateState, delivery_id: &str) -> Result<(), GateError> {
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    let now = now_nanos();
    conn.execute(
        "UPDATE deliveries
         SET state = 'failed', error = 'external_rejected', updated_at = ?2,
             acknowledged_at = ?2
         WHERE delivery_id = ?1 AND state = 'sending'",
        params![delivery_id, now],
    )
    .map_err(|_| GateError::store())?;
    Ok(())
}
