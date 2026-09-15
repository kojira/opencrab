#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    delivery_effect, start_session_turn, AgentRuntime, NormalizedInbound, RunRequest,
    SubtaskCompletionSink, TranscriptSource,
};

use crate::completion::{v3_attach_dispatch, ExtgateCompletionSink};
use crate::delivery::apply_delivery_effect;
use crate::delivery_mode::{adjust_inbound_effect, DeliveryMode};
use crate::error::ErrorCode;
use crate::listen::{emit_activity, emit_ended_activity};
use crate::protocol::Said;
use crate::registry::ExtgateState;

use super::asserted_caller;
use super::binding::OriginRow;
use super::record::seq_for_origin;

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
    // Gatewayが指定した外部返信相関。coreは内容を解釈せずsayへ返す。
    reply_target: Option<&str>,
) {
    let locks = runtime.session_locks();
    let session_id = session_id.to_string();
    let agent_id = row.agent_id.clone();
    let instance_id = row.instance_id.clone();
    let binding_id = said.binding_id.clone();
    let author_id = said.author_id.clone();
    let author_label = said.author_label.clone();
    let text = said.text.clone();
    let images = said.image_urls();
    let address = row.address.clone();
    let origin = said.origin.clone();
    let delivery_mode = row.delivery_mode;
    let system_context = system_context.to_string();
    let reply_target = reply_target.map(str::to_string);
    let caller = asserted_caller(&said.caller);
    let only_speaker = said.only_speaker;
    if !state.turn_queues.try_reserve(&session_id) {
        #[cfg(any(test, feature = "extgate-probe"))]
        state
            .probe
            .turn_queue_dropped
            .fetch_add(1, Ordering::SeqCst);
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
                let activity_id = uuid::Uuid::new_v4().to_string();
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
                let system = if system_context.is_empty() {
                    system
                } else {
                    format!("{system}\n\n{system_context}")
                };
                let last_continuation_say = Arc::new(std::sync::Mutex::new(None::<String>));
                let turn_res = {
                    let runtime = runtime.clone();
                    let session_id = session_id.clone();
                    let agent_id = agent_id.clone();
                    let author_id = author_id.clone();
                    let author_label = author_label.clone();
                    let address = address.clone();
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
                    let hook_reply = reply_target.clone();
                    let request_reply_target = reply_target.clone();
                    let hook_last_continuation_say = Arc::clone(&last_continuation_say);
                    tokio::spawn(async move {
                        let inbound = NormalizedInbound {
                            session_id: &session_id,
                            agent_id: &agent_id,
                            sender_id: &author_id,
                            sender_name: author_label.as_deref().unwrap_or(""),
                            avatar_url: None,
                            channel_id: Some(&address),
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
                                delivery_mode,
                                system_context,
                            });
                        start_session_turn(
                            &runtime,
                            TranscriptSource::new("external", "external_response"),
                            &inbound,
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
                                // send_text経路を通し、Sayモードのみ配送する。配送失敗はターンを失敗させる。
                                .with_on_continuation_speech({
                                    let hs = Arc::clone(&hook_state);
                                    let hi = hook_instance.clone();
                                    let hb = hook_binding.clone();
                                    let ha = hook_agent.clone();
                                    let hse = hook_session.clone();
                                    let hr = hook_reply.clone();
                                    let dm = delivery_mode;
                                    let latest = Arc::clone(&hook_last_continuation_say);
                                    Arc::new(move |speech: String| {
                                        let hs = Arc::clone(&hs);
                                        let hi = hi.clone();
                                        let hb = hb.clone();
                                        let ha = ha.clone();
                                        let hse = hse.clone();
                                        let hr = hr.clone();
                                        let latest = Arc::clone(&latest);
                                        Box::pin(async move {
                                            if dm == DeliveryMode::Say {
                                                let delivery_id =
                                                    crate::delivery::deliver_intermediate_say(
                                                        &hs,
                                                        &hi,
                                                        &hb,
                                                        &ha,
                                                        &hse,
                                                        &speech,
                                                        hr.as_deref(),
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
                                            }
                                            Ok(())
                                        })
                                    })
                                });
                                // Gateway が与えた opaque な返信先だけを subtask 完了へ持ち回る。
                                // 内部 origin を外部返信先として推測しない。
                                if let Some(target) = request_reply_target.clone() {
                                    req = req.with_reply_target(target);
                                }
                                // DI 拡張 §8: 宣言能力を tool set へ載せる（宣言があるときだけ）。
                                if let Some(ga) = ops_actions.clone() {
                                    req = req.with_gateway_actions(ga);
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
                    })
                    .await
                };
                match turn_res {
                    Ok(turn) => {
                        let engine_result = turn.as_ref().and_then(|result| result.as_ref().ok());
                        let engine_completion = engine_result.map(|er| {
                            (
                                er.last_posting_utterance_id.clone(),
                                er.stopped_by_limit,
                                er.last_generation_had_continuation_speech,
                            )
                        });
                        let silent_origins = engine_result
                            .map(|er| er.silent_origins.clone())
                            .unwrap_or_default();
                        let engine_failed = turn.as_ref().is_some_and(Result::is_err);
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
                        let effect = adjust_inbound_effect(delivery_mode, effect);
                        // 単一メンションは発端 origin を say payload に明示（gateway が e-tag reply）。
                        // bundle は None（gateway が standalone post で publish・row292）。
                        let final_say_id = apply_delivery_effect(
                            &state,
                            &instance_id,
                            &binding_id,
                            &agent_id,
                            &session_id,
                            effect,
                            reply_target.as_deref(),
                        )
                        .await;
                        // §13.3.1 案E: 進行中判定は**エージェント単位**（別 session の未決着 subtask
                        // も含む）。agent-scope は本 session の subtask も内包するので session-scope の
                        // 上位互換。1 つでも走行中なら idle でない＝completed_target を送らない。
                        let agent_has_running = runtime.has_running_subtask_for_agent(&agent_id);
                        // 選定規則（§13.3.5）は resume ターンと共通なので共有ヘルパへ集約（単一実装）。
                        let completed_target = crate::completion::select_completed_target(
                            engine_completion,
                            agent_has_running,
                            final_say_id,
                            last_continuation_say
                                .lock()
                                .expect("continuation say id lock")
                                .clone(),
                        );
                        // 成功したexecutionだけauthoritative ended outcomeを送る。engine errorは
                        // turn_failed経路が正本であり、empty silenceへ偽装しない。
                        if !engine_failed {
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
