async fn handle_msg(client: &InstanceClient, msg: CoreMsg, generation: u64) -> bool {
    match msg {
        CoreMsg::Say(say) => handle_say(client, say, generation).await,
        CoreMsg::Activity(activity) => handle_activity(client, activity, generation).await,
        CoreMsg::ToolActivity(activity) => {
            handle_tool_activity(client, activity).await;
            false
        }
        CoreMsg::SubtaskLifecycle(lifecycle) => {
            handle_subtask_lifecycle(client, lifecycle).await;
            false
        }
        CoreMsg::TurnFailed(tf) => {
            handle_turn_failed(client, tf).await;
            false
        }
        CoreMsg::Partial(partial) => {
            handle_partial(client, partial).await;
            false
        }
        CoreMsg::ProvisionalResult(result) => {
            handle_provisional_result(client, result).await;
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
    {
        let inner = client.inner.lock().await;
        if inner.closed || inner.generation != generation {
            return true;
        }
    }
    tracing::info!(
        instance_id = %client.instance_id,
        session = %inv.session_key,
        operation = %inv.operation,
        "invoke"
    );
    // Validate the complete hello snapshot before the adapter handler can produce any external
    // effect. This catches stale/missing digests, undeclared operations, policy drift, and
    // guarantee downgrade on the receiving side.
    if let Err(code) = client.validate_invocation(&inv) {
        let _ = send_frame(client, err_frame(&inv.id, code, None)).await;
        return false;
    }
    // handler 未配線（能力ゼロ）なら未宣言 operation として fail-closed で operation_unknown
    // （外部 I/O 0・§5.1）。
    let Some(handler) = &client.invoke_handler else {
        let _ = send_frame(client, err_frame(&inv.id, "operation_unknown", None)).await;
        return false;
    };
    match handler
        .handle(&inv.id, &inv.session_key, &inv.operation, &inv.payload)
        .await
    {
        InvokeOutcome::Ok(result) => {
            // The versioned invocation envelope carries the declaration's generic effect. The
            // client never classifies an operation by name.
            if inv.effect == "utterance" {
                let mut inner = client.inner.lock().await;
                if inner.closed || inner.generation != generation {
                    return true;
                }
                if let Some(turn) = inner.pending_turn.get_mut(&inv.session_key) {
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

async fn handle_say(client: &InstanceClient, say: Say, generation: u64) -> bool {
    tracing::info!(
        instance_id = %client.instance_id,
        session = %say.session_key,
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
    let explicit_reply_target = match say_reply_target(&say.payload) {
        Ok(target) => target.map(str::to_string),
        Err(_) => {
            let _ = send_frame(client, err_frame(&say.id, "bad_request", None)).await;
            return false;
        }
    };
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return true;
    }
    // D-1099: the say names the session by its key; the gateway owns what that key means.
    let address = say.session_key.clone();
    // D-1093: 返信先は payload に明示された reply_target だけ。進行中ターンの発端 said から
    // 推測しない（地の文の say は返信先を持たない）。
    let reply_origin = explicit_reply_target;
    let q = inner
        .live
        .entry(address.clone())
        .or_insert_with(LiveQueue::new);
    let accepted = q.try_push(LiveEvent::Message {
        delivery_id: say.id.clone(),
        text,
        reply_origin,
        stream_id: say_stream_id(&say.payload).map(str::to_string),
    });
    if !accepted {
        drop(inner);
        let _ = send_frame(client, err_frame(&say.id, "external_rejected", None)).await;
        return false;
    }
    if let Some(turn) = inner.pending_turn.get_mut(&say.session_key) {
        turn.saw_utterance = true;
    }
    drop(inner);
    if !send_frame(client, ok_frame(&say.id)).await {
        close_all(client, "disconnect", generation).await;
        return true;
    }
    false
}

async fn handle_activity(client: &InstanceClient, activity: Activity, generation: u64) -> bool {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return true;
    }
    let address = activity.session_key.clone();

    if activity.state == "ended" {
        let legacy_turn = inner.pending_turn.remove(&activity.session_key);
        let has_completed_target = activity.completed_target.is_some();
        let mut events = vec![LiveEvent::Activity {
            activity_id: activity.activity_id,
            state: activity.state,
            origin: activity.origin,
        }];
        if let Some(target) = activity.completed_target {
            events.push(LiveEvent::Completed { target });
        }
        if let Some(origins) = activity.silent_origins {
            // Presentはemptyを含めauthoritative。admission/say観測によるlegacy推測を重ねない。
            let mut emitted = std::collections::HashSet::new();
            for origin in origins {
                if emitted.insert(origin.clone()) {
                    events.push(LiveEvent::CompletedNoReply {
                        reply_origin: Some(origin),
                    });
                }
            }
        } else if !has_completed_target {
            // Field欠落だけ旧coreとしてstandalone-turn推測へfallbackする。
            if let Some(turn) = legacy_turn.filter(|turn| !turn.saw_utterance) {
                let reply_origin = match turn.reply_origin {
                    ReplyOrigin::Single(origin) => Some(origin),
                    ReplyOrigin::None | ReplyOrigin::Ambiguous => None,
                };
                events.push(LiveEvent::CompletedNoReply { reply_origin });
            }
        }
        let accepted = inner
            .live
            .entry(address)
            .or_insert_with(LiveQueue::new)
            .try_push_batch(events);
        if !accepted {
            drop(inner);
            close_all(client, "live_queue_full", generation).await;
            return true;
        }
        return false;
    }

    let accepted = inner
        .live
        .entry(address)
        .or_insert_with(LiveQueue::new)
        .try_push(LiveEvent::Activity {
            activity_id: activity.activity_id,
            state: activity.state.clone(),
            origin: activity.origin,
        });
    if !accepted {
        drop(inner);
        close_all(client, "live_queue_full", generation).await;
        return true;
    }
    if activity.state == "started" {
        inner
            .pending_turn
            .entry(activity.session_key)
            .or_insert_with(|| PendingTurn {
                saw_utterance: false,
                reply_origin: ReplyOrigin::None,
            });
    }
    false
}

/// D-1087: ツール活動を、そのセッションキーの live queue へ積む。
///
/// id を持たない通知なので応答しない。見ない consumer は無視してよく、core の挙動は変わらない。
async fn handle_tool_activity(client: &InstanceClient, activity: ToolActivity) {
    let address = activity.session_key.clone();
    push_live(
        client,
        &address,
        LiveEvent::ToolActivity {
            activity_id: activity.activity_id,
            call_id: activity.call_id,
            sub_session_id: activity.sub_session_id,
            tool: activity.tool,
            status: activity.status,
            depth: activity.depth,
            started_at: activity.started_at,
            duration_ms: activity.duration_ms,
            args: activity.args,
            result: activity.result,
            error: activity.error,
        },
    )
    .await;
}

/// D-1087: サブタスク走行のライフサイクルを live queue へ積む（応答しない）。
async fn handle_subtask_lifecycle(client: &InstanceClient, lifecycle: SubtaskLifecycle) {
    let address = lifecycle.session_key.clone();
    push_live(
        client,
        &address,
        LiveEvent::SubtaskLifecycle {
            run_id: lifecycle.run_id,
            sub_session_id: lifecycle.sub_session_id,
            status: lifecycle.status,
            label: lifecycle.label,
            task: lifecycle.task,
            detail: lifecycle.detail,
            duration_ms: lifecycle.duration_ms,
            result_text: lifecycle.result_text,
        },
    )
    .await;
}

/// live queue へ 1 件積む。溢れたら best-effort で捨てる（partial と同じ扱い）。
async fn push_live(client: &InstanceClient, address: &str, event: LiveEvent) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let queue = inner
        .live
        .entry(address.to_string())
        .or_insert_with(LiveQueue::new);
    if !queue.try_push(event) {
        tracing::debug!(%address, "activity live event dropped: queue full");
    }
}

async fn handle_turn_failed(client: &InstanceClient, tf: TurnFailed) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let q = inner
        .live
        .entry(tf.session_key).or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::TurnFailed {
        reply_origin: tf.origin,
    });
}

/// D-1083: core→gate partial text. It has no id, so nothing is written back; a full live queue
/// drops it without closing the connection.
async fn handle_partial(client: &InstanceClient, partial: Partial) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let q = inner
        .live
        .entry(partial.session_key).or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::Partial {
        stream_id: partial.stream_id,
        text: partial.text,
        provisional_id: partial.provisional_id,
    });
}

/// D-1085: core→gate provisional result. Id-less like a partial.
async fn handle_provisional_result(client: &InstanceClient, result: ProvisionalResult) {
    let mut inner = client.inner.lock().await;
    if inner.closed {
        return;
    }
    let q = inner
        .live
        .entry(result.session_key).or_insert_with(LiveQueue::new);
    let _ = q.try_push(LiveEvent::ProvisionalResult {
        provisional_id: result.provisional_id,
        promoted: result.promoted,
    });
}

async fn handle_response(client: &InstanceClient, resp: WireResponse, generation: u64) {
    let mut inner = client.inner.lock().await;
    if inner.closed || inner.generation != generation {
        return;
    }
    if let Some(pending) = inner.pending_commands.remove(&resp.id) {
        let outcome = if resp.ok {
            resp.result.ok_or(CommandError::Disconnected)
        } else {
            Err(CommandError::Rejected {
                code: resp.code.unwrap_or_else(|| "bad_request".to_string()),
                message: resp.message.or(resp.detail),
            })
        };
        let _ = pending.reply.send(outcome);
        return;
    }
    if inner.expired_commands.remove(&resp.id) {
        return;
    }
    let Some(pending) = inner.pending_said.remove(&resp.id) else {
        drop(inner);
        close_all(client, "response_invalid", generation).await;
        return;
    };
    let outcome = match pending.kind {
        PendingKind::Open => {
            if resp.ok {
                match resp.session_id {
                    Some(session_id) => SaidOutcome::Opened { session_id },
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
        PendingKind::Hello | PendingKind::Command => {
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
    if let Some(stop) = inner.stop.take() {
        let _ = stop.send(());
    }
    for (_, pending) in inner.pending_said.drain() {
        let _ = pending.reply.send(SaidOutcome::Disconnected);
    }
    for (_, pending) in inner.pending_commands.drain() {
        let _ = pending.reply.send(Err(CommandError::Disconnected));
    }
    inner.expired_commands.clear();
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

