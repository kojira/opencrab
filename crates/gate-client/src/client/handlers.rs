async fn handle_msg(client: &InstanceClient, msg: CoreMsg, generation: u64) -> bool {
    match msg {
        CoreMsg::Bind(bind) => {
            handle_bind(client, bind, generation).await;
            false
        }
        CoreMsg::Say(say) => handle_say(client, say, generation).await,
        CoreMsg::Activity(activity) => {
            handle_activity(client, activity).await;
            false
        }
        CoreMsg::TurnFailed(tf) => {
            handle_turn_failed(client, tf).await;
            false
        }
        CoreMsg::Invoke(inv) => handle_invoke(client, inv, generation).await,
        CoreMsg::Response(resp) => {
            handle_response(client, resp, generation).await;
            false
        }
        CoreMsg::Reverse { id, .. } | CoreMsg::Unknown { id, .. } => {
            if let Some(id) = id {
                let _ = send_frame(client, err_frame(&id, "unknown_message", None)).await;
            }
            false
        }
        CoreMsg::Invalid { id, code, .. } => {
            if let Some(id) = id {
                let _ = send_frame(client, err_frame(&id, code, None)).await;
            }
            if code == "response_invalid" {
                close_all(client, "response_invalid", generation).await;
                return true;
            }
            false
        }
    }
}

/// invoke を実行して応答する。戻り値は「接続を閉じるべきか」（Indeterminate のとき true）。
async fn handle_invoke(client: &InstanceClient, inv: Invoke, generation: u64) -> bool {
    tracing::info!(
        instance_id = %client.instance_id,
        binding_id = %inv.binding_id,
        operation = %inv.operation,
        "invoke"
    );
    // handler 未配線（能力ゼロ）なら未宣言 operation として fail-closed で operation_unknown
    // （外部 I/O 0・§5.1）。
    let Some(handler) = &client.invoke_handler else {
        let _ = send_frame(client, err_frame(&inv.id, "operation_unknown", None)).await;
        return false;
    };
    match handler
        .handle(&inv.id, &inv.binding_id, &inv.operation, &inv.payload)
        .await
    {
        InvokeOutcome::Ok(result) => {
            // #900: 発話クラス（reply/reaction/repost）の invoke が Ok で決着したら、進行中ターンを
            // 「発話あり」に印づける。これで ended 時に沈黙（CompletedNoReply → 🤐）を立てない。
            // 照会・操作クラス（resolve 等）は is_utterance=false なので印づけない（沈黙判定は不変）。
            if handler.is_utterance(&inv.operation) {
                let mut inner = client.inner.lock().await;
                if let Some(turn) = inner.pending_turn.get_mut(&inv.binding_id) {
                    turn.saw_utterance = true;
                }
            }
            let _ = send_frame(client, invoke_ok_frame(&inv.id, &result)).await;
            false
        }
        InvokeOutcome::Rejected => {
            let _ = send_frame(client, err_frame(&inv.id, "operation_rejected", None)).await;
            false
        }
        InvokeOutcome::Indeterminate => {
            // 受理不明: 応答を作らず接続を閉じる。core は EOF を見て pending invoke を
            // indeterminate/disconnect にする（§5.3・不明を確定拒否へ捏造しない）。
            close_all(client, "invoke_indeterminate", generation).await;
            true
        }
    }
}

async fn handle_bind(client: &InstanceClient, bind: Bind, generation: u64) {
    tracing::info!(
        instance_id = %client.instance_id,
        binding_id = %bind.binding_id,
        address = %bind.address,
        "bind"
    );
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    if let Some(existing) = inner.acknowledged.get(&bind.address) {
        if existing != &bind.binding_id {
            drop(inner);
            close_all(client, "binding_conflict", generation).await;
            return;
        }
    }
    inner
        .remembered
        .insert(bind.address.clone(), bind.binding_id.clone());
    inner
        .acknowledged
        .insert(bind.address.clone(), bind.binding_id);
    inner
        .live
        .entry(bind.address)
        .or_insert_with(LiveQueue::new);
    drop(inner);
    let _ = send_frame(client, ok_frame(&bind.id)).await;
}

async fn handle_say(client: &InstanceClient, say: Say, generation: u64) -> bool {
    tracing::info!(
        instance_id = %client.instance_id,
        binding_id = %say.binding_id,
        "say"
    );
    if client.say_policy == SayPolicy::RejectExternal {
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    }
    let Some(text) = say_text(&say.payload).map(str::to_string) else {
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    };
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return true;
    }
    let address = inner
        .acknowledged
        .iter()
        .find(|(_, bid)| *bid == &say.binding_id)
        .map(|(a, _)| a.clone());
    let Some(address) = address else {
        drop(inner);
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    };
    // 返信先: payload の明示 reply_target（送信側が載せた発端 origin・resume 等）を最優先。
    // 無ければ進行中ターンの pending_turn（即時 said が刻んだ Single だけ Some）に委ねる。
    let reply_origin = say_reply_target(&say.payload)
        .map(str::to_string)
        .or_else(|| match inner.pending_turn.get(&say.binding_id) {
            Some(turn) => match &turn.reply_origin {
                ReplyOrigin::Single(o) => Some(o.clone()),
                ReplyOrigin::None | ReplyOrigin::Ambiguous => None,
            },
            None => None,
        });
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let accepted = q.try_push(LiveEvent::Message {
        delivery_id: say.id.clone(),
        text,
        reply_origin,
    });
    if !accepted {
        drop(inner);
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    }
    if let Some(turn) = inner.pending_turn.get_mut(&say.binding_id) {
        turn.saw_utterance = true;
    }
    drop(inner);
    if !send_frame(client, ok_frame(&say.id)).await {
        close_all(client, "disconnect", generation).await;
        return true;
    }
    false
}

async fn handle_activity(client: &InstanceClient, activity: Activity) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let address = inner
        .acknowledged
        .iter()
        .find(|(_, bid)| *bid == &activity.binding_id)
        .map(|(a, _)| a.clone());
    let Some(address) = address else {
        return;
    };
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::Activity {
        activity_id: activity.activity_id,
        state: activity.state.clone(),
        origin: activity.origin.clone(),
    });
    if activity.state == "started" {
        inner
            .pending_turn
            .entry(activity.binding_id.clone())
            .or_insert_with(|| PendingTurn {
                saw_utterance: false,
                reply_origin: ReplyOrigin::None,
            });
    } else if activity.state == "ended" {
        if let Some(target) = activity.completed_target {
            if let Some(q) = inner.live.get_mut(&address) {
                let _ = q.try_push(LiveEvent::Completed { target });
            }
            inner.pending_turn.remove(&activity.binding_id);
            return;
        }
        if let Some(turn) = inner.pending_turn.remove(&activity.binding_id) {
            if !turn.saw_utterance {
                // Message と同じ pending_turn.reply_origin を露出する（新フレームではなく既存追跡の
                // surface）。Single だけ発端を運び、None/Ambiguous は単一発端無しとして None。
                let reply_origin = match &turn.reply_origin {
                    ReplyOrigin::Single(o) => Some(o.clone()),
                    ReplyOrigin::None | ReplyOrigin::Ambiguous => None,
                };
                if let Some(q) = inner.live.get_mut(&address) {
                    let _ = q.try_push(LiveEvent::CompletedNoReply { reply_origin });
                }
            }
        }
    }
}

/// R3(❌): core→gate のターン失敗通知を live queue へ載せる。binding_id→address を解決し、
/// 未 ack binding は捨てる（handle_activity と同じ経路）。id を持たない通知なので応答は返さない。
async fn handle_turn_failed(client: &InstanceClient, tf: TurnFailed) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let address = inner
        .acknowledged
        .iter()
        .find(|(_, bid)| *bid == &tf.binding_id)
        .map(|(a, _)| a.clone());
    let Some(address) = address else {
        return;
    };
    let q = inner.live.entry(address).or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::TurnFailed {
        reply_origin: tf.origin,
    });
}

async fn handle_response(client: &InstanceClient, resp: WireResponse, generation: u64) {
    let mut inner = client.inner.lock().await;
    let Some(pending) = inner.pending_said.remove(&resp.id) else {
        drop(inner);
        close_all(client, "response_invalid", generation).await;
        return;
    };
    let outcome = match pending.kind {
        PendingKind::Hello => {
            if resp.ok && resp.seq.is_none() {
                SaidOutcome::Accepted { seq: 0 }
            } else if !resp.ok {
                SaidOutcome::WireErr {
                    code: resp.code.unwrap_or_else(|| "bad_request".into()),
                    detail: resp.detail,
                }
            } else {
                drop(inner);
                close_all(client, "response_invalid", generation).await;
                return;
            }
        }
        PendingKind::Said => {
            if resp.ok {
                match resp.seq {
                    Some(Some(seq)) => SaidOutcome::Accepted { seq },
                    Some(None) => SaidOutcome::NotAdmitted,
                    None => {
                        drop(inner);
                        close_all(client, "response_invalid", generation).await;
                        return;
                    }
                }
            } else {
                SaidOutcome::WireErr {
                    code: resp.code.unwrap_or_else(|| "bad_request".into()),
                    detail: resp.detail,
                }
            }
        }
    };
    let _ = pending.reply.send(outcome);
}

async fn close_all(client: &InstanceClient, code: &str, generation: u64) {
    let mut inner = client.inner.lock().await;
    if inner.closed || inner.generation != generation {
        return;
    }
    inner.closed = true;
    let drained: Vec<(String, String)> = inner.acknowledged.drain().collect();
    for (address, binding_id) in drained {
        inner.remembered.insert(address, binding_id);
    }
    for (_, pending) in inner.pending_said.drain() {
        let _ = pending.reply.send(SaidOutcome::Disconnected);
    }
    inner.pending_turn.clear();
    let ev = LiveEvent::Error {
        code: code.to_string(),
        detail: None,
    };
    for q in inner.live.values_mut() {
        let _ = q.try_push(ev.clone());
        q.waiters.clear();
    }
    drop(inner);
    tracing::info!(instance_id = %client.instance_id, code, "close");
    client.closed_notify.notify_waiters();
}

