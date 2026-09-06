use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::gateway::DiscordGateway;
use opencrab_actions::{delivery_effect, run_session_turn, DeliveryEffect};

use crate::AgentRunner;

use super::reactions::{
    add_reaction_non_fatal, end_of_speech_qualifies_ok, prepend_runtime_context_discord,
    FAILED_EMOJI, NO_REPLY_EMOJI, SPOKE_EMOJI,
};
use super::{discord_context_line, LoopEvent, ReactionAdder};

/// エージェント応答結果を処理してDiscordに送信する。
///
/// `gateway` / `channel_id` / `message_id` は `NO_REPLY` の可視化（#317）にだけ使う。
/// `message_id` は元のユーザー投稿の Discord ID（空なら付与をスキップ）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_agent_response<T: AgentRunner, G: ReactionAdder>(
    effect: DeliveryEffect,
    agent_id: &str,
    session_id: &str,
    channel_id: u64,
    channel_id_str: &str,
    state: &T,
    gateway: &G,
    message_id: &str,
) {
    match effect {
        DeliveryEffect::NoReply => {
            debug!(agent_id = %agent_id, "Agent returned NO_REPLY");
            // #899: 沈黙は speech を残さない（裸 NO_REPLY を永続すると typed 履歴へ再注入される）。
            // 黙ったことを投稿者に見せる（#317）。失敗しても応答処理は続けない
            // ＝ NO_REPLY のまま終わるのは変わらない。
            add_reaction_non_fatal(
                gateway,
                channel_id,
                channel_id_str,
                message_id,
                NO_REPLY_EMOJI,
            )
            .await;
        }
        DeliveryEffect::Text {
            body,
            tool_calls_made,
            ..
        } => {
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id,
                    session_id,
                    channel_id: Some(channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::Direct { tool_calls_made }),
                },
            );
        }
        DeliveryEffect::Empty => debug!(agent_id = %agent_id, "Agent produced empty response"),
        DeliveryEffect::Failed { error } => {
            error!(agent_id = %agent_id, error = %error, "SkillEngine failed");
            // #668: ターンが失敗したことを、トリガー投稿への ❌ リアクションだけで可視化する。
            // **エラー本文はチャンネルへ出さない**（複数エージェントが居るチャンネルで互いの
            // エラー文に反応し合う無限ループを防ぐ。詳細はログ＝#665 の計装と llm_logs が持つ）。
            // ここに来るのはターンにつき 1 回・最終 Result の Err なので、エンジン内リトライ
            // （#667）が決着した後の**最終失敗時のみ**付く（途中のリトライには付かない）。
            // 付与失敗自体は add_reaction_non_fatal が warn ログで握る（それ以上連鎖しない）。
            add_reaction_non_fatal(
                gateway,
                channel_id,
                channel_id_str,
                message_id,
                FAILED_EMOJI,
            )
            .await;
        }
    }
}

/// サブタスク完了イベントを処理する（P2: イベントループで直列実行）。
///
/// `caller` は subtask を spawn した**元のターンの呼び出し元**（#298）。resume は
/// 元の会話の続きなので、ここで最小権限へ落とすと owner/trusted のツールが
/// `policy_allows` で list_tools からも dispatch からも消える。引き継ぐだけで、
/// 昇格はしない（元が `Agent` のターンは `Agent` のまま）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_subtask_completed<T: AgentRunner>(
    session_id: String,
    agent_id: String,
    subtask_id: String,
    _result: String,
    exit_reason: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
    caller: opencrab_actions::CallerIdentity,
) {
    // #352: 本ターンの caller で index を絞る（resume は元ターンの caller を引き継ぐ）。
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id, &caller);

    // Get task description from subtask session
    let task_description = {
        let sub_session_id = format!("subtask-{}", subtask_id);
        state
            .session_theme(&sub_session_id)
            .map(|theme| {
                // theme is "Subtask: {task}", strip the prefix
                theme
                    .strip_prefix("Subtask: ")
                    .unwrap_or(&theme)
                    .to_string()
            })
            .unwrap_or_default()
    };

    let system_prompt = format!(
        "{}\n\n{}\n[subtask_completed: subtask_id={}, task=\"{}\", exit_reason={}]",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        subtask_id,
        task_description,
        exit_reason
    );
    // #431: resume ターンが**さらに** subtask を投げたら、そこにも「発言終わり」は
    // 付けず次の resume へ委ねる。通常経路と同じカウンタの張り方。
    let subtask_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    debug!(agent_id = %agent_id, session_id = %session_id, stage = "context_build", "turn: 文脈構築 開始（入）");
    let Some(run_result) = run_session_turn(
        &state,
        &session_id,
        &agent_id,
        &system_prompt,
        &prepend_runtime_context_discord("", "Discord conversation", ""),
        |raw| {
            debug!(agent_id = %agent_id, session_id = %session_id, conversation_len = raw.len(), stage = "context_build", "turn: 文脈構築 完了（出）");
            prepend_runtime_context_discord(raw, "Discord conversation", "")
        },
        |conversation| {
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                caller,
            )
            .with_subtask_starts(subtask_starts.clone())
            .with_gateway_actions(gateway_actions)
            .with_reply_target(channel_id_str.clone())
            .with_dispatch(Some(subtask_registry.clone()), {
                let sink: std::sync::Arc<dyn opencrab_actions::SubtaskCompletionSink> =
                    std::sync::Arc::new(crate::gateway_actions::DiscordCompletionSink {
                        event_tx: Some(event_tx.clone()),
                    });
                sink
            })
        },
    )
    .await
    else {
        return;
    };
    match delivery_effect(
        run_result,
        opencrab_actions::DeliveryContext {
            session_id: &session_id,
            agent_id: &agent_id,
            origin: "discord",
        },
    ) {
        DeliveryEffect::NoReply => {
            // #899: 沈黙は speech を残さない。
        }
        DeliveryEffect::Text {
            body,
            stopped_by_limit,
            ..
        } => {
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            let sent_id = match gateway.send_to_channel(channel_id, &body).await {
                Ok(id) => {
                    if let Some(v) = &voice {
                        v.maybe_speak(&channel_id_str, &agent_id, &body);
                    }
                    id
                }
                Err(e) => {
                    error!("Subtask completion Discord send failed: {e}");
                    None
                }
            };
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id: &agent_id,
                    session_id: &session_id,
                    channel_id: Some(&channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::SubtaskCompleted),
                },
            );
            // #431: 自然終了（NO_REPLY/空は上で return 済み）かつ打ち切りでなく、実際に
            // 投稿できたなら「発言終わり」を付ける。判定は通常経路と同じゲートへ寄せる。
            // この経路は 1 応答 1 送信なので `posted` = 送信 id の有無。付与失敗は non-fatal。
            if end_of_speech_qualifies_ok(
                stopped_by_limit,
                sent_id.is_some(),
                subtask_starts.load(std::sync::atomic::Ordering::SeqCst) > 0,
            ) {
                if let Some(id) = sent_id {
                    add_reaction_non_fatal(
                        gateway.as_ref(),
                        channel_id,
                        &channel_id_str,
                        &id.to_string(),
                        SPOKE_EMOJI,
                    )
                    .await;
                }
            }
        }
        _ => {}
    }
}

/// 時刻起因の発火（#588 TimedFire）を**いつもの Discord ターン**として処理する。
///
/// `SubtaskCompleted` の resume と同型（受信を記録せず system プロンプトへマーカー/プロンプトを足して
/// ターンを回す）だが、初回発火は「宣言 → `spawn_subtask` → 最終 `NO_REPLY`」の形になるので、通常の
/// 受信ターンと同じく **`on_response_text` で反復ごとに配送**する（そうしないと最終が `NO_REPLY` のとき
/// 宣言がチャンネルに出ない）。継続ターンは `with_dispatch`（`DiscordCompletionSink` → `SubtaskCompleted`）
/// でループ既存の resume 経路に載る（ハートビート専用の継続機構は不要）。
///
/// **受け口は薄い**: 送信・ロック・記録・継続はすべてループ既存の実装。ここが担うのは
/// 「渡された prompt を system プロンプトへ足して回す」だけ。プロンプトは会話ログに「発言」として
/// 残さない（#501）。時刻発火の沈黙（`NO_REPLY`）は無記録（ハートビートの決定を踏襲。通常の受信ターンは
/// NO_REPLY マーカーを残すが、ここは発火元メッセージが無いのでマーカー行を積み上げない）。
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_timed_fire<T: AgentRunner>(
    session_id: String,
    agent_id: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    is_dm: bool,
    prompt: String,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
    caller: opencrab_actions::CallerIdentity,
) {
    // 時刻発火の受信ログ（#588）。送信側（scheduler）の「発火」ログと突き合わせれば、
    // scheduler→この Discord ループ間で落ちたかが分かる。heartbeat 専用の文言にしない。
    tracing::info!(
        agent_id = %agent_id,
        session_id = %session_id,
        transport = "discord",
        is_dm,
        prompt_preview = %opencrab_actions::prompt_preview(&prompt),
        "timed-fire: ターン開始（Discord loop 受信）"
    );
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id, &caller);
    // 渡された prompt（#584 指示解決の結果など）は system プロンプトへ足す（通常ターンの
    // discord_context_line も付ける）。会話ログには「発言」として残さない（#501）。
    let system_prompt = format!(
        "{}\n\n{}\n\n{}",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        prompt
    );
    // 反復ごとに応答テキストを配送する（通常の受信ターンと同じ on_response_text）。宣言が出る要。
    // NO_REPLY・空はスキップ。書き込み不可チャンネルもスキップ。発火を塞がないよう spawn。
    let on_response_text: Arc<dyn Fn(String) + Send + Sync> = {
        let gateway = gateway.clone();
        let state = state.clone();
        let voice = voice.clone();
        let channel_id_str = channel_id_str.clone();
        let agent_id = agent_id.clone();
        Arc::new(move |text: String| {
            // 第一柱: NO_REPLY 終端解釈で前段のみ配送（空・単独 NO_REPLY はスキップ）。
            // 破棄ログは最終応答を判定する delivery_effect が出す（反復途中での二重計上を避ける）。
            let text = match opencrab_actions::terminate_at_no_reply(&text).speech() {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => return,
            };
            // #890 §11.7: 最終行 CONTINUE 単独を剥がす（継続判定は engine 済み・表示保護）。
            let text = opencrab_actions::strip_trailing_continue(&text)
                .map(str::to_string)
                .unwrap_or(text);
            if text.trim().is_empty() {
                return;
            }
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            let gateway = gateway.clone();
            let voice = voice.clone();
            let channel_id_str = channel_id_str.clone();
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                match gateway.send_to_channel(channel_id, &text).await {
                    Ok(_) => {
                        if let Some(v) = &voice {
                            v.maybe_speak(&channel_id_str, &agent_id, &text);
                        }
                    }
                    Err(e) => tracing::error!("TimedFire Discord send failed: {e}"),
                }
            });
        })
    };

    debug!(agent_id = %agent_id, session_id = %session_id, stage = "context_build", "turn: 文脈構築 開始（入）");
    let Some(result) = run_session_turn(
        &state,
        &session_id,
        &agent_id,
        &system_prompt,
        &prepend_runtime_context_discord("", "Discord conversation", ""),
        |raw| {
            debug!(agent_id = %agent_id, session_id = %session_id, conversation_len = raw.len(), stage = "context_build", "turn: 文脈構築 完了（出）");
            prepend_runtime_context_discord(raw, "Discord conversation", "")
        },
        |conversation| {
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                caller,
            )
            .with_gateway_actions(gateway_actions)
            .with_reply_target(channel_id_str.clone())
            .with_on_response_text(on_response_text)
            .with_dispatch(Some(subtask_registry.clone()), {
                let sink: std::sync::Arc<dyn opencrab_actions::SubtaskCompletionSink> =
                    std::sync::Arc::new(crate::gateway_actions::DiscordCompletionSink {
                        event_tx: Some(event_tx.clone()),
                    });
                sink
            })
        },
    )
    .await
    else {
        return;
    };

    // 記録（配送は on_response_text が済ませているので送信はしない）。最終応答が NO_REPLY 以外なら
    // 通常ターンと同じ record_outbound_reply。沈黙は無記録（上記 doc）。
    match delivery_effect(
        result,
        opencrab_actions::DeliveryContext {
            session_id: &session_id,
            agent_id: &agent_id,
            origin: "discord",
        },
    ) {
        DeliveryEffect::Text {
            body,
            tool_calls_made,
            ..
        } => {
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id: &agent_id,
                    session_id: &session_id,
                    channel_id: Some(&channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::Direct { tool_calls_made }),
                },
            );
        }
        DeliveryEffect::Failed { error } => {
            error!(agent_id = %agent_id, error = %error, "TimedFire turn failed");
        }
        DeliveryEffect::NoReply | DeliveryEffect::Empty => {}
    }
}
