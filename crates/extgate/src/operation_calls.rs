//! generic operation call の永続と terminal 化（DI 拡張 §5 / §7 / §10.6・option B）。
//!
//! wire 層は operation/payload/result を opaque JSON として扱い、platform 意味を解釈しない。
//! `invoke_and_wait` は背景 subtask（`SubtaskToolDispatcher` が spawn 済み）から呼ばれ、invoke を
//! 送って wire 応答（handle_response）または close（indeterminate）を oneshot で await する。turn は
//! 既に `{"status":"spawned",...}` で返っており（常時 detach）、この await は subtask 内なので detach を
//! 壊さない。await 完了で execute() が戻り、既存 dispatch_batch が settle_completed を発火する（DI-08）。
//! invoke の再送は 0 回。

use std::sync::Arc;

use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::protocol::{invoke_frame, write_json};
use crate::registry::{ExtgateState, OperationOutcome, Pending};

/// invoke の確定結果としての失敗（§5.3）。code は generic stable value。
#[derive(Debug, Clone)]
pub struct InvokeError {
    pub code: ErrorCode,
    pub detail: Option<String>,
}

impl InvokeError {
    fn new(code: ErrorCode) -> Self {
        Self { code, detail: None }
    }
}

/// 背景 subtask 内で呼ぶ（§5.2 / §10.6・option B）。call を `sending` で insert → commit →
/// pending 登録 → invoke を exact 1 回 write → wire 応答 / close の terminal outcome を await する。
///
/// - ok(result) → `Ok(result)`（result は opaque JSON。null 含む）。
/// - err(operation_rejected) → `Err(operation_rejected)`。
/// - write/EOF/protocol close/ack 不明 → `Err(disconnect)`（call は indeterminate）。
///
/// commit 前は wire write 0。宣言に無い operation は call/pending/wire 0 で `operation_unknown`。
pub async fn invoke_and_wait(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    operation: &str,
    payload: &Value,
) -> Result<Value, InvokeError> {
    // §10.6 step 1: live + acknowledged binding + live declaration を再検査。
    let writer = {
        let reg = state
            .lock_registry()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let live = reg
            .get(instance_id)
            .ok_or_else(|| InvokeError::new(ErrorCode::NotConnected))?;
        if !live.acknowledged.contains(binding_id) {
            return Err(InvokeError::new(ErrorCode::NotConnected));
        }
        if live.declaration(operation).is_none() {
            return Err(InvokeError::new(ErrorCode::OperationUnknown));
        }
        live.writer.clone()
    };

    let call_id = Uuid::new_v4().to_string();
    let now = now_nanos();
    let payload_text =
        serde_json::to_string(payload).map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
    {
        let mut conn = state
            .db
            .lock()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let open = tx.query_row(
            "SELECT instance_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
            params![binding_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        );
        match open {
            Ok((inst, None)) if inst == instance_id => {}
            Ok(_) => {
                let _ = tx.rollback();
                return Err(InvokeError::new(ErrorCode::BindingClosed));
            }
            Err(_) => {
                let _ = tx.rollback();
                return Err(InvokeError::new(ErrorCode::StoreError));
            }
        }
        tx.execute(
            "INSERT INTO gateway_operation_calls
               (call_id, binding_id, operation, payload_json, result_json, state, error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, 'sending', NULL, ?5, ?5)",
            params![call_id, binding_id, operation, payload_text, now],
        )
        .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        tx.commit()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
    }

    // commit 後だけ pending へ登録（terminal outcome を届ける oneshot 付き）。
    let (reply_tx, reply_rx) = oneshot::channel();
    {
        let mut reg = state
            .lock_registry()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let Some(live) = reg.get_mut(instance_id) else {
            drop(reg);
            let _ = terminalize_call(state, &call_id, &OperationOutcome::Indeterminate);
            return Err(InvokeError::new(ErrorCode::Disconnect));
        };
        live.pending.insert(
            call_id.clone(),
            Pending::Invoke {
                call_id: call_id.clone(),
                binding_id: binding_id.to_string(),
                operation: operation.to_string(),
                reply: reply_tx,
            },
        );
    }

    // background writer へ invoke を exact 1 回。失敗は connection close（pending invoke は
    // close 側で indeterminate 化＋oneshot に Indeterminate 送出）。
    if write_json(
        &writer,
        &invoke_frame(&call_id, binding_id, operation, None, payload),
    )
    .await
    .is_err()
    {
        crate::close::close_live(
            state,
            Some(instance_id),
            None,
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(InvokeError::new(ErrorCode::Disconnect));
    }

    match reply_rx.await {
        Ok(OperationOutcome::Succeeded { result_json }) => {
            Ok(serde_json::from_str(&result_json).unwrap_or(Value::Null))
        }
        Ok(OperationOutcome::Failed) => Err(InvokeError::new(ErrorCode::OperationRejected)),
        Ok(OperationOutcome::Indeterminate) | Err(_) => {
            Err(InvokeError::new(ErrorCode::Disconnect))
        }
    }
}

/// 発話クラス（撃ちっぱなし・DESIGN-RESUME-SETTLE §3.3.1 C5）の invoke。
///
/// `invoke_and_wait` と違い **operation_call を作らず**、say と同型の crash-safe delivery で
/// 永続する: 1 TX で発話本文を `speech` ログとして残し（本文＋関係注記のみ・機械行なし C6）、
/// `deliveries` 行を `sending` で立て、commit 後に `Pending::Utterance` を登録して invoke を
/// exact 1 回 write する。**await しない**（settle/resume を起こさない）。gateway 応答は
/// `handle_response` が deliveries を terminal 化し、失敗のみ `turn_failed`/❌ で表面化する（C9）。
///
/// `speech_body` は永続する発話本文（本文や絵文字など・空可）。`utterance_kind` は関係注記の
/// 種別（発話 op 名。既知名 → 本文/種別/対象の写像は `opencrab_gateway::utterance_body`）。
/// `reply_target_id` は会話内 e 番号解決用の 64hex（metadata）、`reply_target_origin` は ❌ 付与用
/// の発端 origin。
#[allow(clippy::too_many_arguments)]
pub async fn invoke_utterance(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    agent_id: &str,
    session_id: &str,
    operation: &str,
    payload: &Value,
    speech_body: &str,
    utterance_kind: &str,
    reply_target_id: Option<&str>,
    reply_target_origin: Option<&str>,
) -> Result<(), InvokeError> {
    // live + acknowledged binding + live declaration を再検査（invoke_and_wait と同じ）。
    let writer = {
        let reg = state
            .lock_registry()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let live = reg
            .get(instance_id)
            .ok_or_else(|| InvokeError::new(ErrorCode::NotConnected))?;
        if !live.acknowledged.contains(binding_id) {
            return Err(InvokeError::new(ErrorCode::NotConnected));
        }
        if live.declaration(operation).is_none() {
            return Err(InvokeError::new(ErrorCode::OperationUnknown));
        }
        live.writer.clone()
    };

    let call_id = Uuid::new_v4().to_string();
    let delivery_id = Uuid::new_v4().to_string();
    let now = now_nanos();
    // deliveries.payload_json は `{"text": ...}` 形が CHECK 制約（say と共用のテーブル）。発話本文を
    // 載せる（wire への invoke は別途 invoke_frame で原 payload を送る・二重には持たない）。
    let delivery_payload = serde_json::json!({ "text": speech_body }).to_string();
    // 発話本文の関係注記用 metadata（C6: レンダリングは本文＋関係注記のみ）。
    let mut speech_meta = serde_json::Map::new();
    speech_meta.insert(
        "source".to_string(),
        Value::String(
            opencrab_actions::TranscriptSource::External
                .reply()
                .to_string(),
        ),
    );
    speech_meta.insert(
        "utterance_kind".to_string(),
        Value::String(utterance_kind.to_string()),
    );
    if let Some(t) = reply_target_id {
        speech_meta.insert("reply_target".to_string(), Value::String(t.to_string()));
    }
    let speech_meta = Value::Object(speech_meta).to_string();

    {
        let mut conn = state
            .db
            .lock()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let open = tx.query_row(
            "SELECT instance_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
            params![binding_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        );
        match open {
            Ok((inst, None)) if inst == instance_id => {}
            Ok(_) => {
                let _ = tx.rollback();
                return Err(InvokeError::new(ErrorCode::BindingClosed));
            }
            Err(_) => {
                let _ = tx.rollback();
                return Err(InvokeError::new(ErrorCode::StoreError));
            }
        }
        // 発話本文を speech として永続（撃ちっぱなしでも「言った」ことは残る＝復唱の抑止源）。
        opencrab_db::queries::insert_session_log(
            &tx,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: speech_body.to_string(),
                speaker_id: Some(agent_id.to_string()),
                turn_number: None,
                metadata_json: Some(speech_meta),
                created_at: None,
            },
        )
        .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        tx.execute(
            "INSERT INTO deliveries (delivery_id, binding_id, payload_json, state, error, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'sending', NULL, ?4, ?4)",
            params![delivery_id, binding_id, delivery_payload, now],
        )
        .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        tx.commit()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
    }

    // commit 後だけ pending 登録（terminal は deliveries 行・oneshot 無し＝resume を起こさない）。
    {
        let mut reg = state
            .lock_registry()
            .map_err(|_| InvokeError::new(ErrorCode::StoreError))?;
        let Some(live) = reg.get_mut(instance_id) else {
            drop(reg);
            let _ = crate::delivery::mark_indeterminate(state, std::slice::from_ref(&delivery_id));
            return Err(InvokeError::new(ErrorCode::Disconnect));
        };
        live.pending.insert(
            delivery_id.clone(),
            Pending::Utterance {
                delivery_id: delivery_id.clone(),
                binding_id: binding_id.to_string(),
                reply_target: reply_target_origin.map(str::to_string),
            },
        );
    }

    // invoke を exact 1 回 write（wire への invoke は従来どおり・gateway が publish）。
    if write_json(
        &writer,
        &invoke_frame(&call_id, binding_id, operation, None, payload),
    )
    .await
    .is_err()
    {
        let _ = crate::delivery::mark_indeterminate(state, std::slice::from_ref(&delivery_id));
        crate::close::close_live(
            state,
            Some(instance_id),
            None,
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(InvokeError::new(ErrorCode::Disconnect));
    }
    Ok(())
}

/// call を terminal 化する（§5.3）。DB のみ。terminal 遷移は
/// `sending→succeeded|failed|indeterminate` で、既に terminal の行は変えない。
/// 成功捏造を避けるため DB 失敗は Err を返す（呼び出し側が close + halt）。
pub fn terminalize_call(
    state: &ExtgateState,
    call_id: &str,
    outcome: &OperationOutcome,
) -> Result<(), GateError> {
    let now = now_nanos();
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    match outcome {
        OperationOutcome::Succeeded { result_json } => conn.execute(
            "UPDATE gateway_operation_calls
             SET state = 'succeeded', result_json = ?2, error = NULL, updated_at = ?3
             WHERE call_id = ?1 AND state = 'sending'",
            params![call_id, result_json, now],
        ),
        OperationOutcome::Failed => conn.execute(
            "UPDATE gateway_operation_calls
             SET state = 'failed', result_json = NULL, error = 'operation_rejected', updated_at = ?2
             WHERE call_id = ?1 AND state = 'sending'",
            params![call_id, now],
        ),
        OperationOutcome::Indeterminate => conn.execute(
            "UPDATE gateway_operation_calls
             SET state = 'indeterminate', result_json = NULL, error = 'disconnect', updated_at = ?2
             WHERE call_id = ?1 AND state = 'sending'",
            params![call_id, now],
        ),
    }
    .map_err(|_| GateError::store())?;
    Ok(())
}

/// connection close で drain した pending invoke を `indeterminate/disconnect` にし、各 await へ
/// Indeterminate を届ける。
pub fn close_pending_invokes(
    state: &ExtgateState,
    invokes: Vec<(String, oneshot::Sender<OperationOutcome>)>,
) {
    for (call_id, reply) in invokes {
        if let Err(e) = terminalize_call(state, &call_id, &OperationOutcome::Indeterminate) {
            tracing::error!(
                code = e.code.as_str(),
                "operation call indeterminate failed"
            );
        }
        let _ = reply.send(OperationOutcome::Indeterminate);
    }
}

/// startup recover（§7.5）。HTTP/UDS より前に exact 1 回。残 `sending` を stale indeterminate に
/// する。restart 後は in-memory subtask が無いので決着 handoff は行わず DB terminal 化のみ。
pub fn recover_stale_calls(conn: &mut Connection, now: i64) -> Result<(), GateError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    tx.execute(
        "UPDATE gateway_operation_calls
         SET state = 'indeterminate',
             error = 'stale sending recovered after restart',
             updated_at = ?1
         WHERE state = 'sending'",
        params![now],
    )
    .map_err(|_| GateError::store())?;
    tx.commit().map_err(|_| GateError::store())?;
    Ok(())
}
