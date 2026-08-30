//! generic operation call の永続と terminal 化（DI 拡張 §5 / §7 / §10.6）。
//!
//! wire 層は operation/payload/result を opaque JSON として扱い、platform 意味を解釈しない。
//! call を terminal 化した後、`state.fire_operation_settlement` で projection の
//! settle_completed 経路へ決着を渡す（DI-08）。invoke の再送は 0 回。

use std::sync::Arc;

use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::Value;

use crate::error::{ErrorCode, GateError};
use crate::ids::now_nanos;
use crate::protocol::{invoke_frame, write_json};
use crate::registry::{ExtgateState, OperationOutcome, OperationSettlement, Pending};

/// wire-side dispatch（§5.2 / §10.6）。call を `sending` で insert → commit → pending 登録
/// → invoke を exact 1 回 write。projection の dispatcher とテストの双方から呼べる。
///
/// commit 前は wire write 0。enqueue/write 失敗は call を `indeterminate/disconnect` にし、
/// 決着を settle hook へ渡して connection を close する。
pub async fn enqueue_invoke(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    call_id: &str,
    operation: &str,
    payload: &Value,
) -> Result<(), GateError> {
    // §10.6 step 1: live + acknowledged binding を再検査。不成立なら call/pending/wire write 0。
    let writer = {
        let reg = state.lock_registry()?;
        let live = reg
            .get(instance_id)
            .ok_or_else(|| GateError::new(ErrorCode::NotConnected))?;
        if !live.acknowledged.contains(binding_id) {
            return Err(GateError::new(ErrorCode::NotConnected));
        }
        // live declaration に無い operation は call/wire 0（§5.1）。
        if live.declaration(operation).is_none() {
            return Err(GateError::new(ErrorCode::OperationUnknown));
        }
        live.writer.clone()
    };

    let now = now_nanos();
    let payload_text = serde_json::to_string(payload).map_err(|_| GateError::store())?;
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
        tx.execute(
            "INSERT INTO gateway_operation_calls
               (call_id, binding_id, operation, payload_json, result_json, state, error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, 'sending', NULL, ?5, ?5)",
            params![call_id, binding_id, operation, payload_text, now],
        )
        .map_err(|_| GateError::store())?;
        tx.commit().map_err(|_| GateError::store())?;
    }

    // commit 後だけ pending map へ登録し、background writer へ invoke を渡す。
    {
        let mut reg = state.lock_registry()?;
        let Some(live) = reg.get_mut(instance_id) else {
            drop(reg);
            settle_call(
                state,
                call_id,
                binding_id,
                operation,
                OperationOutcome::Indeterminate,
            )?;
            return Err(GateError::new(ErrorCode::Disconnect));
        };
        live.pending.insert(
            call_id.to_string(),
            Pending::Invoke {
                call_id: call_id.to_string(),
                binding_id: binding_id.to_string(),
                operation: operation.to_string(),
            },
        );
    }

    if write_json(
        &writer,
        &invoke_frame(call_id, binding_id, operation, None, payload),
    )
    .await
    .is_err()
    {
        settle_call(
            state,
            call_id,
            binding_id,
            operation,
            OperationOutcome::Indeterminate,
        )?;
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
    Ok(())
}

/// call を terminal 化し、決着を settle hook へ渡す（DI-08）。terminal 遷移は
/// `sending→succeeded|failed|indeterminate` のみで、既に terminal の行は変えない。
/// DB 失敗は success を捏造せず Err を返す（呼び出し側が close + halt）。
pub fn settle_call(
    state: &ExtgateState,
    call_id: &str,
    binding_id: &str,
    operation: &str,
    outcome: OperationOutcome,
) -> Result<(), GateError> {
    let now = now_nanos();
    let updated = {
        let conn = state.db.lock().map_err(|_| GateError::store())?;
        match &outcome {
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
        .map_err(|_| GateError::store())?
    };
    // 既に terminal（updated=0）の call は決着を重複発火しない。
    if updated == 0 {
        return Ok(());
    }
    state.fire_operation_settlement(OperationSettlement {
        call_id: call_id.to_string(),
        binding_id: binding_id.to_string(),
        operation: operation.to_string(),
        outcome,
    });
    Ok(())
}

/// connection close で当該 pending invoke 群を `indeterminate/disconnect` にし、各決着を渡す。
pub fn settle_calls_indeterminate(
    state: &ExtgateState,
    calls: &[(String, String, String)],
) {
    for (call_id, binding_id, operation) in calls {
        if let Err(e) = settle_call(
            state,
            call_id,
            binding_id,
            operation,
            OperationOutcome::Indeterminate,
        ) {
            tracing::error!(code = e.code.as_str(), "operation call indeterminate failed");
        }
    }
}

/// startup recover（§7.5）。HTTP/UDS より前に exact 1 回。残 `sending` を stale
/// indeterminate にする。決着 handoff は projection（session 復元が要る）に委ねるため、
/// ここでは DB terminal 化だけを行い、recover した call を返す。
pub fn recover_stale_calls(
    conn: &mut Connection,
    now: i64,
) -> Result<Vec<(String, String, String)>, GateError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let recovered: Vec<(String, String, String)> = {
        let mut stmt = tx
            .prepare(
                "SELECT call_id, binding_id, operation
                 FROM gateway_operation_calls WHERE state = 'sending'",
            )
            .map_err(|_| GateError::store())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|_| GateError::store())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|_| GateError::store())?);
        }
        out
    };
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
    Ok(recovered)
}
