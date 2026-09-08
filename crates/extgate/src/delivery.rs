//! DeliveryEffect → reply + sending 同一 TX、その後 say 1 回。V3 §6.3 / §7.4。

#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{DeliveryEffect, TranscriptSource};
use opencrab_db::queries::{insert_session_log, SessionLogRow};
use rusqlite::{params, TransactionBehavior};
use uuid::Uuid;

use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::listen::emit_turn_failed;
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
            // #899: 沈黙（NO_REPLY 終端）は speech を残さない。裸 NO_REPLY を永続すると
            // conversation_typed が `assistant: 'NO_REPLY'` としてモデルへ再注入する。
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

/// #898 §12.2/§13.1: 末尾 CONTINUE で継続する途中イテレーションの発話を、最終応答と同じ
/// 経路（[`send_text`] = say 配送＋memory_sessions speech 保存）で 1 件配送・保存する。
/// engine の継続分岐フックがループ中に await し、`Err` は継続を止める（§13.1 j: 失敗を隠さない）。
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
    let writer = {
        let reg = state.lock_registry()?;
        let live = reg
            .get(instance_id)
            .ok_or_else(|| GateError::new(ErrorCode::NotConnected))?;
        if !live.acknowledged.contains(binding_id) {
            return Err(GateError::new(ErrorCode::NotConnected));
        }
        live.writer.clone()
    };

    let delivery_id = Uuid::new_v4().to_string();
    let now = now_nanos();
    let payload = serde_json::json!({"text": body}).to_string();
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
                    serde_json::json!({"source": TranscriptSource::External.reply()}).to_string(),
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
            "INSERT INTO deliveries (delivery_id, binding_id, payload_json, state, error, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'sending', NULL, ?4, ?4)",
            params![delivery_id, binding_id, payload, now],
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

    let write_err = write_json(
        &writer,
        &say_frame(&delivery_id, binding_id, body, reply_target),
    )
    .await
    .is_err();
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
             SET state = 'indeterminate', error = 'disconnect', updated_at = ?2
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
         SET state = 'delivered', error = NULL, updated_at = ?2
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
         SET state = 'failed', error = 'external_rejected', updated_at = ?2
         WHERE delivery_id = ?1 AND state = 'sending'",
        params![delivery_id, now],
    )
    .map_err(|_| GateError::store())?;
    Ok(())
}
