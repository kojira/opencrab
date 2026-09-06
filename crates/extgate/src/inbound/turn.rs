#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::Ordering;
use std::sync::Arc;

use opencrab_actions::{
    delivery_effect, start_session_turn, AgentRuntime, CallerIdentity, NormalizedInbound,
    RunRequest, SubtaskCompletionSink, TranscriptSource,
};
use opencrab_db::queries::{TRUSTED_PLATFORM_EXTGATE, TRUSTED_PLATFORM_NOSTR};

use crate::completion::{v3_attach_dispatch, ExtgateCompletionSink};
use crate::delivery::apply_delivery_effect;
use crate::delivery_mode::{adjust_inbound_effect, DeliveryMode};
use crate::error::ErrorCode;
use crate::listen::emit_activity;
use crate::protocol::Said;
use crate::registry::{ExtgateState, NostrHeldTurn};
use crate::ResolveCallerFn;

use super::binding::OriginRow;
use super::record::seq_for_origin;

#[allow(clippy::too_many_arguments)]
pub(super) fn enqueue_turn<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
    row: &OriginRow,
    said: &Said,
    session_id: &str,
    prompt_suffix: &str,
    // #933: この said の external_origins.seq。dequeue 時に「fold 済み集合に含まれる」なら独立
    // ターンを skip する（二重処理防止・非消費）。bundle は個別 said でないので None（skip 対象外）。
    seq: Option<i64>,
    // 単一メンション turn は発端 said の origin へ返信（say payload の reply_target に載せる）。
    // bundle turn は単一返信先が無いので false（gateway が standalone post で publish・row292）。
    // 返信先は say payload の明示 reply_target を正とする（裁定A で ended は say の後になったが、
    // gateway の pending_turn 相関に依存させず明示値を主にする方針は据え置き）。
    reply_to_origin: bool,
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
    let kind_id = row.kind_id.clone();
    let delivery_mode = row.delivery_mode;
    let prompt_suffix = prompt_suffix.to_string();
    // say の返信先（発端イベント origin）。単一メンションのみ。bundle は None。
    let reply_target: Option<String> = if reply_to_origin {
        Some(origin.clone())
    } else {
        None
    };
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
                // R2(👀): started に発端 origin を載せる。gateway はこの時点で 👀 を付ける
                // （record-only/held はここへ来ないので「読まれるまで付かない」が保たれる）。
                emit_activity(
                    &state,
                    &instance_id,
                    &binding_id,
                    &activity_id,
                    "started",
                    Some(origin.as_str()),
                    None,
                )
                .await;
                let caller = match state.db.lock() {
                    Ok(conn) => resolve_caller(
                        &conn,
                        if kind_id == "nostr" {
                            TRUSTED_PLATFORM_NOSTR
                        } else {
                            TRUSTED_PLATFORM_EXTGATE
                        },
                        &[author_id.as_str()],
                        &agent_id,
                        &owner_id,
                    ),
                    Err(_) => CallerIdentity::Agent,
                };
                let (system, name) = runtime.build_agent_context(&agent_id, &caller);
                let system = if prompt_suffix.is_empty() {
                    system
                } else {
                    format!("{system}\n\n{prompt_suffix}")
                };
                let subtask_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let last_continuation_say = Arc::new(std::sync::Mutex::new(None::<String>));
                let turn_res = {
                    let runtime = runtime.clone();
                    let session_id = session_id.clone();
                    let agent_id = agent_id.clone();
                    let author_id = author_id.clone();
                    let address = address.clone();
                    let text = text.clone();
                    let images = images.clone();
                    let origin = origin.clone();
                    let kind_id = kind_id.clone();
                    let state = Arc::clone(&state);
                    let instance_id = instance_id.clone();
                    let binding_id = binding_id.clone();
                    let prompt_suffix = prompt_suffix.clone();
                    // #898: 継続分岐の途中発話フック用クローン（state/instance/binding は直後に
                    // sink へ move されるので、フック用に別クローンを先に確保する）。
                    let hook_state = Arc::clone(&state);
                    let hook_instance = instance_id.clone();
                    let hook_binding = binding_id.clone();
                    let hook_agent = agent_id.clone();
                    let hook_session = session_id.clone();
                    let hook_reply = reply_target.clone();
                    let hook_subtask_starts = Arc::clone(&subtask_starts);
                    let hook_last_continuation_say = Arc::clone(&last_continuation_say);
                    tokio::spawn(async move {
                        let inbound = NormalizedInbound {
                            session_id: &session_id,
                            agent_id: &agent_id,
                            sender_id: &author_id,
                            sender_name: "",
                            avatar_url: None,
                            channel_id: Some(&address),
                            pubkey: if kind_id == "nostr" {
                                Some(author_id.as_str())
                            } else {
                                None
                            },
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
                                kind_id: kind_id.clone(),
                                author_id: author_id.clone(),
                                delivery_mode,
                                prompt_suffix,
                            });
                        start_session_turn(
                            &runtime,
                            TranscriptSource::External,
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
                                // 発端イベントの origin を subtask へ引き継ぐ。subtask 完了時の
                                // resume ターンの say がこの origin へ返信できるようにする
                                // （settlement→SubtaskSettled.reply_target 経由）。
                                .with_reply_target(origin.clone())
                                .with_subtask_starts(Arc::clone(&hook_subtask_starts))
                                // #930: 走行中に畳み込んだ said を LLM へ渡す時点で read+origin を emit
                                // （👀 を返信の前に付ける）。同時にその origin を「畳み込み済み」に記録し、
                                // 後で dequeue するその said 自身の独立ターンを起こさない（第2欠陥）。
                                .with_on_read_origin({
                                    let hs = Arc::clone(&hook_state);
                                    let hi = hook_instance.clone();
                                    let hb = hook_binding.clone();
                                    let hse = hook_session.clone();
                                    Arc::new(move |origin: String| {
                                        let hs = Arc::clone(&hs);
                                        let hi = hi.clone();
                                        let hb = hb.clone();
                                        let hse = hse.clone();
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
                                            // （非消費）。以後この seq 以下の独立ターンは skip される。
                                            if let Some(seq) = seq_for_origin(&hs, &hb, &origin) {
                                                hs.mark_folded_seq(&hse, seq);
                                            }
                                        })
                                    })
                                })
                                // #898 §12.2/§13.1 j: 末尾 CONTINUE の途中発話をループ中に配送・保存する。
                                // 最終応答と同じ経路（send_text = say 配送＋speech 保存）を通し、Say モード
                                // のみ配送（ToolDriven は say 抑止＝reply DI operation が配送を担う）。
                                // 配送失敗（Err）は継続を止めてターンを失敗させる（失敗を隠さない）。
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
                                // DI 拡張 §8: 宣言能力を tool set へ載せる（宣言があるときだけ）。
                                if let Some(ga) = ops_actions.clone() {
                                    req = req.with_gateway_actions(ga);
                                }
                                v3_attach_dispatch(
                                    req,
                                    &kind_id,
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
                        let engine_completion = turn.as_ref().and_then(|r| {
                            r.as_ref().ok().map(|er| {
                                (
                                    er.last_posting_utterance_id.clone(),
                                    er.stopped_by_limit,
                                    er.last_generation_had_continuation_speech,
                                )
                            })
                        });
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
                        let started_subtask =
                            subtask_starts.load(std::sync::atomic::Ordering::SeqCst) > 0;
                        // §13.3.1 案E: 進行中判定は**エージェント単位**（別 session の未決着 subtask
                        // も含む）。agent-scope は本 session の subtask も内包するので session-scope の
                        // 上位互換。1 つでも走行中なら idle でない＝completed_target を送らない。
                        let agent_has_running = runtime.has_running_subtask_for_agent(&agent_id);
                        // 選定規則（§13.3.5）は resume ターンと共通なので共有ヘルパへ集約（単一実装）。
                        let completed_target = crate::completion::select_completed_target(
                            engine_completion,
                            started_subtask,
                            agent_has_running,
                            final_say_id,
                            last_continuation_say
                                .lock()
                                .expect("continuation say id lock")
                                .clone(),
                        );
                        // 決着（say/reply/no_reply）の配送**後**に activity ended を出す（統括裁定A
                        // 2026-08-31）。これで say フレームが ended より先に gateway へ届き、返信ターンは
                        // saw_say=true になってから ended を見るので、gate-client の CompletedNoReply が
                        // 沈黙（say 無し）ターンだけに正しく立つ（返信ターンでの偽 CompletedNoReply を撤去）。
                        emit_activity(
                            &state,
                            &instance_id,
                            &binding_id,
                            &activity_id,
                            "ended",
                            None,
                            completed_target.as_deref(),
                        )
                        .await;
                    }
                    Err(_) => {
                        tracing::error!("extgate turn task panicked");
                        // パニック時は決着を配送できない。turn 境界だけは通知してから close する
                        // （沈黙ターンと同じく say 無しの ended＝CompletedNoReply 相当）。
                        emit_activity(
                            &state,
                            &instance_id,
                            &binding_id,
                            &activity_id,
                            "ended",
                            None,
                            None,
                        )
                        .await;
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

pub(super) fn fire_held_turns<R: AgentRuntime>(
    state: Arc<ExtgateState>,
    runtime: R,
    resolve_caller: ResolveCallerFn,
    held: Vec<(NostrHeldTurn, CallerIdentity)>,
) {
    let Some((last, _)) = held.last() else {
        return;
    };
    let said = Said {
        id: String::new(),
        binding_id: last.binding_id.clone(),
        origin: last.origin.clone(),
        author_id: last.author_id.clone(),
        text: last.text.clone(),
        attachments: last.images.clone(),
    };
    let row = OriginRow {
        instance_id: last.instance_id.clone(),
        kind_id: last.kind_id.clone(),
        address: last.address.clone(),
        agent_id: last.agent_id.clone(),
        owner_id: last.owner_id.clone(),
        delivery_mode: last.delivery_mode,
    };
    // #933: 保留していた said 自身の seq。fold 済み集合に在れば独立ターンを skip。external_origins に
    // 無ければ None（skip/prune 対象外）。
    let held_seq = seq_for_origin(&state, &said.binding_id, &said.origin);
    enqueue_turn(
        state,
        runtime,
        resolve_caller,
        &row,
        &said,
        &last.session_id,
        &last.prompt_suffix,
        held_seq,
        true, // 保留していた単一メンション: 発端 said の origin へ返信
    );
}
