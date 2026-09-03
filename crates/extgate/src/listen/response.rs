use std::sync::Arc;

use tokio::sync::oneshot;

use super::activity::emit_turn_failed;
use crate::close::close_live;
use crate::delivery::{mark_delivered, mark_failed, mark_indeterminate};
use crate::error::ErrorCode;
use crate::operation_calls::terminalize_call;
use crate::protocol::WireResponse;
use crate::registry::{ExtgateState, OperationOutcome, Pending};

pub(crate) async fn handle_response(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    instance_id: &str,
    identity: u64,
    resp: WireResponse,
) -> Result<(), ()> {
    let pending = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => return Err(()),
        };
        let Some(live) = reg.get_mut(instance_id) else {
            return Err(());
        };
        live.pending.remove(&resp.id)
    };
    let Some(pending) = pending else {
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::ResponseInvalid,
            Some(&resp.id),
            Some(writer),
        )
        .await;
        return Err(());
    };
    match pending {
        Pending::Bind { binding_id, .. } => {
            if resp.ok {
                if resp.seq.is_some() {
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::ResponseInvalid,
                        Some(&resp.id),
                        Some(writer),
                    )
                    .await;
                    return Err(());
                }
                let mut reg = match state.lock_registry() {
                    Ok(g) => g,
                    Err(_) => return Err(()),
                };
                if let Some(live) = reg.get_mut(instance_id) {
                    live.acknowledged.insert(binding_id);
                }
                Ok(())
            } else if resp.code == Some(ErrorCode::BindFailed) {
                close_live(
                    state,
                    Some(instance_id),
                    Some(identity),
                    ErrorCode::BindFailed,
                    None,
                    None,
                )
                .await;
                Err(())
            } else {
                close_live(
                    state,
                    Some(instance_id),
                    Some(identity),
                    ErrorCode::ResponseInvalid,
                    Some(&resp.id),
                    Some(writer),
                )
                .await;
                Err(())
            }
        }
        Pending::Say { delivery_id } => {
            if resp.ok {
                if resp.seq.is_some() {
                    if let Err(e) = mark_indeterminate(state, &[delivery_id]) {
                        tracing::error!(
                            code = e.code.as_str(),
                            "indeterminate after invalid say ok"
                        );
                        state.halt();
                    }
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::ResponseInvalid,
                        Some(&resp.id),
                        Some(writer),
                    )
                    .await;
                    return Err(());
                }
                if let Err(e) = mark_delivered(state, &delivery_id) {
                    tracing::error!(code = e.code.as_str(), "delivered write failed");
                    if let Err(ind) = mark_indeterminate(state, std::slice::from_ref(&delivery_id))
                    {
                        tracing::error!(
                            code = ind.code.as_str(),
                            "indeterminate after delivered write failed"
                        );
                    }
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::StoreError,
                        None,
                        None,
                    )
                    .await;
                    state.halt();
                    return Err(());
                }
                Ok(())
            } else if resp.code == Some(ErrorCode::ExternalRejected) {
                if let Err(e) = mark_failed(state, &delivery_id) {
                    tracing::error!(code = e.code.as_str(), "failed write failed");
                    if let Err(ind) = mark_indeterminate(state, std::slice::from_ref(&delivery_id))
                    {
                        tracing::error!(
                            code = ind.code.as_str(),
                            "indeterminate after failed write failed"
                        );
                    }
                    close_live(
                        state,
                        Some(instance_id),
                        Some(identity),
                        ErrorCode::StoreError,
                        None,
                        None,
                    )
                    .await;
                    state.halt();
                    return Err(());
                }
                Ok(())
            } else {
                if let Err(e) = mark_indeterminate(state, &[delivery_id]) {
                    tracing::error!(
                        code = e.code.as_str(),
                        "indeterminate after invalid say err"
                    );
                    state.halt();
                }
                close_live(
                    state,
                    Some(instance_id),
                    Some(identity),
                    ErrorCode::ResponseInvalid,
                    Some(&resp.id),
                    Some(writer),
                )
                .await;
                Err(())
            }
        }
        Pending::Utterance {
            delivery_id,
            binding_id,
            reply_target,
        } => {
            handle_utterance_response(
                state,
                writer,
                instance_id,
                identity,
                &resp,
                &delivery_id,
                &binding_id,
                reply_target.as_deref(),
            )
            .await
        }
        Pending::Invoke { call_id, reply, .. } => {
            handle_invoke_response(state, writer, instance_id, identity, &resp, &call_id, reply)
                .await
        }
    }
}

/// 発話クラス（撃ちっぱなし・§3.3.1 C5/C9）の invoke 応答。`Say` と同型で deliveries を
/// terminal 化する（operation_call も oneshot も無い＝settle/resume を起こさない）。失敗
/// （external_rejected）だけ `turn_failed`/❌ で表面化し、モデルへ領収書は返さない。
#[allow(clippy::too_many_arguments)]
async fn handle_utterance_response(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    instance_id: &str,
    identity: u64,
    resp: &WireResponse,
    delivery_id: &str,
    binding_id: &str,
    reply_target: Option<&str>,
) -> Result<(), ()> {
    if resp.ok {
        // 発話配送 ok は結果本文を持たない（invoke ok は result 必須だが発話は読まない）。
        // seq は持たない。seq 付き ok は response_invalid（say と同じ厳密さ）。
        if resp.seq.is_some() {
            if let Err(e) =
                mark_indeterminate(state, std::slice::from_ref(&delivery_id.to_string()))
            {
                tracing::error!(
                    code = e.code.as_str(),
                    "indeterminate after invalid utterance ok"
                );
                state.halt();
            }
            close_live(
                state,
                Some(instance_id),
                Some(identity),
                ErrorCode::ResponseInvalid,
                Some(&resp.id),
                Some(writer),
            )
            .await;
            return Err(());
        }
        if let Err(e) = mark_delivered(state, delivery_id) {
            tracing::error!(code = e.code.as_str(), "utterance delivered write failed");
            let _ = mark_indeterminate(state, std::slice::from_ref(&delivery_id.to_string()));
            close_live(
                state,
                Some(instance_id),
                None,
                ErrorCode::StoreError,
                None,
                None,
            )
            .await;
            state.halt();
            return Err(());
        }
        Ok(())
    } else if resp.code == Some(ErrorCode::OperationRejected) {
        // 発話は wire 上 invoke frame で送るため、gateway の確定拒否は say の `external_rejected`
        // ではなく invoke の `operation_rejected` で返る（gate-client handle_invoke）。
        if let Err(e) = mark_failed(state, delivery_id) {
            tracing::error!(code = e.code.as_str(), "utterance failed write failed");
            let _ = mark_indeterminate(state, std::slice::from_ref(&delivery_id.to_string()));
            close_live(
                state,
                Some(instance_id),
                None,
                ErrorCode::StoreError,
                None,
                None,
            )
            .await;
            state.halt();
            return Err(());
        }
        // C9: 発話失敗を発端 origin つきで gateway へ通知する（❌ を付ける）。error 本文は
        // wire に載せない（#668）。単一メンションのみ Some（bundle/曖昧は None）。
        if let Some(origin) = reply_target {
            emit_turn_failed(state, instance_id, binding_id, origin).await;
        }
        Ok(())
    } else {
        if let Err(e) = mark_indeterminate(state, std::slice::from_ref(&delivery_id.to_string())) {
            tracing::error!(
                code = e.code.as_str(),
                "indeterminate after invalid utterance err"
            );
            state.halt();
        }
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::ResponseInvalid,
            Some(&resp.id),
            Some(writer),
        )
        .await;
        Err(())
    }
}

/// invoke 応答の terminal 化（§5.3 / §10.2・option B）。DB を terminal 化してから oneshot で
/// 背景 subtask の `invoke_and_wait` へ outcome を届ける。ok は result を持ち seq を持たない。
/// err は `operation_rejected` だけが failed で、他 code（operation_unknown 含む・DI-07 保留）は
/// `operation_rejected` へ丸めず response_invalid + close とする。
async fn handle_invoke_response(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    instance_id: &str,
    identity: u64,
    resp: &WireResponse,
    call_id: &str,
    reply: oneshot::Sender<OperationOutcome>,
) -> Result<(), ()> {
    // 応答 shape/code から terminal outcome を決める（reply を移動する前に決定）。
    let outcome: Option<OperationOutcome> = if resp.ok {
        // invoke ok は result present（null 含む）で seq を持たない（§10.2）。
        let good_shape = resp.seq.is_none() && resp.result.is_some();
        resp.result
            .as_ref()
            .filter(|_| good_shape)
            .map(|result_value| {
                // JSON null は SQL NULL ではなく text 'null' として保存（§7.1）。
                let result_json =
                    serde_json::to_string(result_value).unwrap_or_else(|_| "null".to_string());
                OperationOutcome::Succeeded { result_json }
            })
    } else if resp.code == Some(ErrorCode::OperationRejected) {
        Some(OperationOutcome::Failed)
    } else {
        // operation_rejected 以外（operation_unknown 含む・DI-07 保留）は丸めず response_invalid。
        None
    };

    let Some(outcome) = outcome else {
        // response-invalid: call を indeterminate 化し、await へ Indeterminate を届けて close。
        if let Err(e) = terminalize_call(state, call_id, &OperationOutcome::Indeterminate) {
            tracing::error!(
                code = e.code.as_str(),
                "indeterminate after invalid invoke response"
            );
            state.halt();
        }
        let _ = reply.send(OperationOutcome::Indeterminate);
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::ResponseInvalid,
            Some(&resp.id),
            Some(writer),
        )
        .await;
        return Err(());
    };

    // terminal 化して outcome を届ける。DB 失敗は成功を捏造せず Indeterminate を届けて close+halt。
    if let Err(e) = terminalize_call(state, call_id, &outcome) {
        tracing::error!(
            code = e.code.as_str(),
            "operation call terminal write failed"
        );
        let _ = reply.send(OperationOutcome::Indeterminate);
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::StoreError,
            None,
            None,
        )
        .await;
        state.halt();
        return Err(());
    }
    let _ = reply.send(outcome);
    Ok(())
}
