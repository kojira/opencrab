#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    delivery_effect, start_session_turn, AgentRuntime, NormalizedInbound, RunRequest,
    SubtaskCompletionSink, TranscriptSource, TurnModelProfile,
};

use crate::completion::{v3_attach_dispatch, ExtgateCompletionSink};
use crate::delivery::apply_delivery_effect;
use crate::error::ErrorCode;
use crate::interrupt::{finish_interrupted, InterruptedTurn, TurnRegistration};
use crate::listen::{emit_activity, emit_ended_activity};
use crate::protocol::Said;
use crate::registry::ExtgateState;

use super::asserted_caller;
use super::session::OriginRow;
use super::record::seq_for_origin;
use crate::provisional::{promote, ProvisionalInputs};

/// D-1085: a promoted provisional run had no read origin, so a silent result names the said's
/// origin as silent, as the normal turn would.
fn silent_if_no_reply(
    mut result: opencrab_core::EngineResult,
    origin: &str,
) -> opencrab_core::EngineResult {
    if result.explicit_termination.is_some()
        && result.response.trim().is_empty()
        && result.silent_origins.is_empty()
    {
        result.silent_origins.push(origin.to_string());
    }
    result
}

/// D-1085 §12: the provisional run ended at a tool outside its read-only set without running it.
fn stopped_at_gated_tool(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<opencrab_core::StoppedAtGatedTool>()
            .is_some()
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn enqueue_turn<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    row: &OriginRow,
    said: &Said,
    session_id: &str,
    system_context: &str,
    // #933: この said の external_origins.seq。dequeue 時に「fold 済み集合に含まれる」なら独立
    // ターンを skip する（二重処理防止・非消費）。bundle は個別 said でないので None（skip 対象外）。
    seq: Option<i64>,
    // この said の origin。失敗通知（turn_failed）の相関にだけ使い、返信先にはしない（D-1093）。
    failure_origin: Option<&str>,
    // D-1085: this said's own memory_sessions rows (inbound, agent prelude).
    own_rows: Vec<i64>,
) {
    let locks = runtime.session_locks();
    let session_id = session_id.to_string();
    let agent_id = row.agent_id.clone();
    let instance_id = row.instance_id.clone();
    let binding_id = row.row_id.clone();
    let author_id = said.author_id.clone();
    let author_label = said.author_label.clone();
    let text = said.text.clone();
    let images = said.image_urls();
    let session_key_value = row.session_key.clone();
    let origin = said.origin.clone();
    let system_context = system_context.to_string();
    let failure_origin = failure_origin.map(str::to_string);
    let caller = asserted_caller(&said.caller);
    let only_speaker = said.only_speaker;
    let model_profile = said.model_profile;
    let provisional_id = said.provisional_id.clone();
    // D-1085: the inputs a provisional run must have used to be promoted by this said. A said
    // with attachments or another profile never promotes.
    let provisional_inputs = (model_profile == TurnModelProfile::Realtime
        && said.attachments.is_empty())
    .then(|| ProvisionalInputs {
        author_id: said.author_id.clone(),
        author_label: said.author_label.clone(),
        caller: said.caller.clone(),
        system_context: system_context.to_string(),
        agent_prelude: said.agent_prelude.clone(),
        text: said.text.clone(),
    });
    if !state.turn_queues.try_reserve(&session_id) {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .turn_queue_dropped
            .fetch_add(1, Ordering::SeqCst);
        if let Some(provisional_id) = &provisional_id {
            state.provisionals.discard(&binding_id, provisional_id);
        }
        return;
    }
    let queues = Arc::clone(&state.turn_queues);
    let session_key = session_id.clone();
    queues.submit(&session_key, async move {
        let lock_id = session_id.clone();
        locks
            .run_serialized(&lock_id, async move {
                // #930/#933: この said が走行中の別ターンへ既に畳み込まれ read 済み（fold 済み集合に
                // seq が在る）なら、独立ターンを起こさない（started も出さず LLM も走らせない）。
                // 畳み込みと独立ターンの二重処理・遅延 👀 の源を断つ（#930 第2欠陥）。#933: 実際に fold
                // した seq だけの非消費集合で判定＝別話者の未 fold said を over-skip しない（OnlySpeaker
                // 対応）・二重 take に免疫。dequeue を機に seq 未満を prune（FIFO なので安全）。
                // skip は fail-loud の観測点として info で残す。bundle（seq=None）は skip 対象外。
                if let Some(seq) = seq {
                    let folded = state.is_folded(&session_id, seq);
                    state.prune_folded_below(&session_id, seq);
                    if folded {
                        if let Some(provisional_id) = &provisional_id {
                            promote::reject(&state, &instance_id, &binding_id, provisional_id)
                                .await;
                        }
                        tracing::info!(
                            session_id = %session_id,
                            origin = %origin,
                            seq,
                            "skip independent turn: said already folded into a running turn (#930/#933)"
                        );
                        return;
                    }
                }
                #[cfg(any(test, feature = "extgate-probe"))]
                state
                    .probe
                    .start_session_turn_count
                    .fetch_add(1, Ordering::SeqCst);
                // D-1096: an `interrupt` of this session stops this turn while it runs.
                let running = TurnRegistration::begin(&state, &session_id, &agent_id);
                let activity_id = uuid::Uuid::new_v4().to_string();
                // D-1087: このターンの活動（ツール実行とサブタスクのライフサイクル）を
                // gateway へ流す stream。activity_id は下の `activity` フレームと同じ値。
                // 読む者が居なければ書くだけ。
                let activity_stream = crate::activity_stream::ActivityEmitter::spawn(
                    Arc::clone(&state),
                    instance_id.clone(),
                    binding_id.clone(),
                    activity_id.clone(),
                );
                // #964: started は typing の開始だけを通知する。発端 origin の read（👀）は、
                // その origin を含む初回 LLM request が完成した後、chat 呼び出し直前に別途 emit する。
                emit_activity(
                    &state,
                    &instance_id,
                    &binding_id,
                    &activity_id,
                    "started",
                    None,
                    None,
                )
                .await;
                let (system, name) = runtime.build_agent_context(&agent_id, &caller);
                let system = crate::completion::with_system_context(system, &system_context);
                // D-1091: この binding の gateway が hello で宣言した環境（名前→値）。core は
                // 解釈せず `execute_shell` の子へ渡すだけ。宣言が無ければ空で従来と同じ。
                let run_env = state
                    .lock_registry()
                    .ok()
                    .and_then(|reg| reg.get(&instance_id).map(|live| live.run_env.clone()))
                    .unwrap_or_default();
                let last_continuation_say = Arc::new(std::sync::Mutex::new(None::<String>));
                // D-1085: decided before delivery; `provisional_result` is written either way.
                let promoted = match (&provisional_id, provisional_inputs) {
                    (Some(provisional_id), Some(inputs)) => {
                        promote::take_promotion(
                            &state,
                            promote::PromotionKey {
                                instance_id: &instance_id,
                                binding_id: &binding_id,
                                provisional_id,
                                session_id: &session_id,
                                inputs,
                                own_rows: &own_rows,
                            },
                        )
                        .await
                    }
                    (Some(provisional_id), None) => {
                        promote::reject(&state, &instance_id, &binding_id, provisional_id).await;
                        None
                    }
                    (None, _) => None,
                };
                // D-1083: only a realtime-profile turn streams partial text; every say of the turn
                // takes the slot. D-1085: a promoted turn delivers on the provisional run's stream.
                let partial_stream = promoted
                    .as_ref()
                    .map(|taken| taken.stream.clone())
                    .unwrap_or_default();
                let partial_hook = (model_profile == TurnModelProfile::Realtime).then(|| {
                    crate::partial_stream::partial_text_hook(
                        Arc::clone(&state),
                        instance_id.clone(),
                        binding_id.clone(),
                        partial_stream.clone(),
                        None,
                    )
                });
                let mut promoted_res = None;
                if let Some(taken) = promoted {
                    // D-1085: the promoted run is this turn's run; its said is now
                    // read like a normal turn's origin.
                    emit_activity(
                        &state,
                        &instance_id,
                        &binding_id,
                        &uuid::Uuid::new_v4().to_string(),
                        "read",
                        Some(origin.as_str()),
                        None,
                    )
                    .await;
                    // §13.2: the run's held continuation speech goes out first, in order.
                    taken
                        .speech
                        .release(crate::provisional::speech::SayTarget {
                            state: Arc::clone(&state),
                            instance_id: instance_id.clone(),
                            binding_id: binding_id.clone(),
                            agent_id: agent_id.clone(),
                            session_id: session_id.clone(),
                            latest: Arc::clone(&last_continuation_say),
                        })
                        .await;
                    running.turn.set_task(taken.task.abort_handle());
                    let res = taken.task.await;
                    // §12: the run reached a gated tool after its promotion and stopped without
                    // running it; the normal turn below runs it (on the same partial stream).
                    if matches!(&res, Ok(Some(Err(e))) if stopped_at_gated_tool(e)) {
                        tracing::info!(
                            session_id = %session_id,
                            origin = %origin,
                            "promoted provisional reran at gated tool"
                        );
                    } else {
                        promoted_res = Some(res.map(|turn| {
                            turn.map(|result| result.map(|er| silent_if_no_reply(er, &origin)))
                        }));
                    }
                }
                // D-1087: ターン終端でキューを書き終えるために手元にも 1 本残す。
                let turn_activity = Arc::clone(&activity_stream);
                let turn_res = if let Some(res) = promoted_res {
                    res
                } else {
                    let runtime = runtime.clone();
                    let session_id = session_id.clone();
                    let agent_id = agent_id.clone();
                    let author_id = author_id.clone();
                    let author_label = author_label.clone();
                    let session_key_value = session_key_value.clone();
                    let text = text.clone();
                    let images = images.clone();
                    let origin = origin.clone();
                    let only_speaker = only_speaker;
                    let state = Arc::clone(&state);
                    let instance_id = instance_id.clone();
                    let binding_id = binding_id.clone();
                    let system_context = system_context.clone();
                    // #898: 継続分岐の途中発話フック用クローン（state/instance/binding は直後に
                    // sink へ move されるので、フック用に別クローンを先に確保する）。
                    let hook_state = Arc::clone(&state);
                    let hook_instance = instance_id.clone();
                    let hook_binding = binding_id.clone();
                    let hook_agent = agent_id.clone();
                    let hook_session = session_id.clone();
                    let hook_last_continuation_say = Arc::clone(&last_continuation_say);
                    let hook_partial_stream = partial_stream.clone();
                    let tracked_tools: Arc<dyn opencrab_actions::ToolEventSink> =
                        Arc::new(crate::interrupt::TrackedTools::new(
                            Arc::clone(&activity_stream) as Arc<dyn opencrab_actions::ToolEventSink>,
                            Arc::clone(&running.turn),
                        ));
                    let engine = tokio::spawn(async move {
                        let inbound = NormalizedInbound {
                            session_id: &session_id,
                            agent_id: &agent_id,
                            sender_id: &author_id,
                            sender_name: author_label.as_deref().unwrap_or(""),
                            avatar_url: None,
                            session_key: Some(&session_key_value),
                            pubkey: None,
                            text: &text,
                            image_urls: &images,
                            external_id: &origin,
                        };
                        let registry = runtime.subtask_registry_for(&session_id);
                        // DI 拡張 §8: 宣言能力を GatewayActions として tool set へ投影する。宣言が
                        // 無ければ None（従来挙動＝能力ゼロ）。state/instance_id/binding_id は直後に
                        // sink へ move するのでここで作る。
                        let ops_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> =
                            crate::ops_projection::ExtgateOpsGatewayActions::for_binding(
                                Arc::clone(&state),
                                &instance_id,
                                &binding_id,
                                &session_id,
                                &agent_id,
                            )
                            .map(|a| Arc::new(a) as Arc<dyn opencrab_gateway::GatewayActions>);
                        let sink: Arc<dyn SubtaskCompletionSink> =
                            Arc::new(ExtgateCompletionSink {
                                state,
                                runtime: runtime.clone(),
                                instance_id,
                                binding_id,
                                agent_id: agent_id.clone(),
                                session_id: session_id.clone(),
                                only_speaker,
                                speaker_id: author_id.clone(),
                                system_context,
                                model_profile,
                                activity: Some(Arc::clone(&activity_stream)),
                            });
                        start_session_turn(
                            &runtime,
                            TranscriptSource::new("external", "external_response"),
                            &inbound,
                            model_profile,
                            &system,
                            // extgate は会話へ runtime context を前置しない（wrap は素通し）。
                            // 予算計上もそれに合わせて空文字（実 request と一致させる契約）。
                            "",
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
                                .with_image_urls(images.clone())
                                // #964: 発端 origin は started へ載せず、初回 LLM request の直前に
                                // read+origin として通知する。
                                .with_initial_read_origin(origin.clone())
                                // #964: 発端と走行中に畳み込んだ said の read+origin を、それぞれを
                                // 含む exact request の `llm.chat` 直前に emit する。畳み込み origin
                                // だけは従来どおり記録し、後続の独立ターンを起こさない。
                                .with_on_read_origin({
                                    let hs = Arc::clone(&hook_state);
                                    let hi = hook_instance.clone();
                                    let hb = hook_binding.clone();
                                    let hse = hook_session.clone();
                                    let initial_origin = origin.clone();
                                    Arc::new(move |origin: String| {
                                        let hs = Arc::clone(&hs);
                                        let hi = hi.clone();
                                        let hb = hb.clone();
                                        let hse = hse.clone();
                                        let initial_origin = initial_origin.clone();
                                        Box::pin(async move {
                                            let activity_id = uuid::Uuid::new_v4().to_string();
                                            crate::listen::emit_activity(
                                                &hs,
                                                &hi,
                                                &hb,
                                                &activity_id,
                                                "read",
                                                Some(origin.as_str()),
                                                None,
                                            )
                                            .await;
                                            // #933: 畳み込んだ said の seq を external_origins から
                                            // 引き、per-session の畳み込み高水位へ単調に記録する
                                            // （非消費）。発端自身は fold ではないので記録しない。
                                            if origin != initial_origin {
                                                if let Some(seq) = seq_for_origin(&hs, &hb, &origin)
                                                {
                                                    hs.mark_folded_seq(&hse, seq);
                                                }
                                            }
                                        })
                                    })
                                })
                                // 明示終了前の途中発話をループ中に配送・保存する。最終応答と同じ
                                // send_text経路を通す。配送失敗はターンを失敗させる。
                                .with_on_continuation_speech({
                                    let hs = Arc::clone(&hook_state);
                                    let hi = hook_instance.clone();
                                    let hb = hook_binding.clone();
                                    let ha = hook_agent.clone();
                                    let hse = hook_session.clone();
                                    let latest = Arc::clone(&hook_last_continuation_say);
                                    let partial_stream = hook_partial_stream.clone();
                                    Arc::new(move |speech: String| {
                                        let hs = Arc::clone(&hs);
                                        let hi = hi.clone();
                                        let hb = hb.clone();
                                        let ha = ha.clone();
                                        let hse = hse.clone();
                                        let latest = Arc::clone(&latest);
                                        let stream_id = partial_stream.take_for_say();
                                        Box::pin(async move {
                                            let delivery_id =
                                                crate::delivery::deliver_intermediate_say(
                                                    &hs,
                                                    &hi,
                                                    &hb,
                                                    &ha,
                                                    &hse,
                                                    &speech,
                                                    stream_id.as_deref(),
                                                )
                                                .await
                                                .map_err(|e| {
                                                    anyhow::anyhow!(
                                                        "extgate intermediate say failed: {}",
                                                        e.code.as_str()
                                                    )
                                                })?;
                                            *latest.lock().expect("continuation say id lock") =
                                                Some(delivery_id);
                                            Ok(())
                                        })
                                    })
                                });
                                if let Some(hook) = partial_hook.clone() {
                                    req = req.with_on_partial_text(hook);
                                }
                                // D-1087: ツール実行を活動 stream に載せる（開始 → 終端の順）。
                                req = req.with_tool_event_sink(Arc::clone(&tracked_tools));
                                // DI 拡張 §8: 宣言能力を tool set へ載せる（宣言があるときだけ）。
                                if let Some(ga) = ops_actions.clone() {
                                    req = req.with_gateway_actions(ga);
                                }
                                // D-1091: gateway が宣言した binding の環境をシェル子へ渡す。
                                if !run_env.is_empty() {
                                    req = req.with_run_env(run_env.clone());
                                }
                                v3_attach_dispatch(
                                    req,
                                    only_speaker,
                                    author_id.clone(),
                                    registry.clone(),
                                    Arc::clone(&sink),
                                )
                            },
                        )
                        .await
                    });
                    running.turn.set_task(engine.abort_handle());
                    engine.await
                };
                // D-1096: an interrupted turn delivers nothing more of its own.
                if let Some(delivered) = running.turn.close() {
                    let say_recorded = last_continuation_say
                        .lock()
                        .expect("continuation say id lock")
                        .is_some();
                    finish_interrupted(
                        &running.turn,
                        InterruptedTurn {
                            state: &state,
                            instance_id: &instance_id,
                            binding_id: &binding_id,
                            session_id: &session_id,
                            activity_id: &activity_id,
                            activity: Some(&turn_activity),
                            say_recorded,
                        },
                        delivered,
                    )
                    .await;
                    return;
                }
                match turn_res {
                    Ok(turn) => {
                        let engine_result = turn.as_ref().and_then(|result| result.as_ref().ok());
                        let engine_completion = engine_result.map(|er| {
                            (
                                er.last_posting_utterance_id.clone(),
                                er.stopped_by_limit,
                                er.last_generation_had_continuation_speech,
                                er.explicit_termination.is_some(),
                            )
                        });
                        let silent_origins = engine_result
                            .map(|er| er.silent_origins.clone())
                            .unwrap_or_default();
                        let engine_succeeded = turn.as_ref().is_some_and(Result::is_ok);
                        let effect = match turn {
                            Some(r) => delivery_effect(
                                r,
                                opencrab_actions::DeliveryContext {
                                    session_id: &session_id,
                                    agent_id: &agent_id,
                                    origin: "extgate",
                                },
                            ),
                            None => opencrab_actions::DeliveryEffect::Empty,
                        };
                        // D-1093: say は返信先を持たない。発端 origin は失敗通知（turn_failed）の
                        // 相関にだけ使う。
                        let final_say_id = apply_delivery_effect(
                            &state,
                            &instance_id,
                            &binding_id,
                            &agent_id,
                            &session_id,
                            effect,
                            failure_origin.as_deref(),
                            partial_stream.take_for_say().as_deref(),
                        )
                        .await;
                        let parent_session_has_running =
                            !runtime.subtask_registry_for(&session_id).is_empty();
                        // 選定規則（§13.3.5）は resume ターンと共通なので共有ヘルパへ集約（単一実装）。
                        let completed_target = crate::completion::select_completed_target(
                            engine_completion,
                            parent_session_has_running,
                            final_say_id,
                            last_continuation_say
                                .lock()
                                .expect("continuation say id lock")
                                .clone(),
                        );
                        // 成功したexecutionだけauthoritative ended outcomeを送る。engine errorは
                        // turn_failed経路が正本であり、empty silenceへ偽装しない。
                        // D-1087: 活動の frame はキュー越しなので、`ended` の前に書き終える。
                        turn_activity.flush().await;
                        if engine_succeeded {
                            emit_ended_activity(
                                &state,
                                &instance_id,
                                &binding_id,
                                &activity_id,
                                completed_target.as_deref(),
                                &silent_origins,
                            )
                            .await;
                        }
                    }
                    Err(_) => {
                        tracing::error!("extgate turn task panicked");
                        // Authoritative engine outcomeを得られないためsuccessful endedは送らない。
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
