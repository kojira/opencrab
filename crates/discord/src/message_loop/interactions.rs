use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::gateway::DiscordGateway;
use opencrab_actions::{delivery_effect, run_session_turn, DeliveryEffect};
use opencrab_core::a2ui::UiRenderer;

use crate::AgentRunner;

use super::reactions::{
    add_reaction_non_fatal, end_of_speech_qualifies_ok, prepend_runtime_context_discord,
    SPOKE_EMOJI,
};
#[cfg(doc)]
use super::turn_completion::process_subtask_completed;
use super::{discord_context_line, LoopEvent};

/// Discordコンポーネントインタラクション（ボタンクリック・セレクトメニュー・モーダルSubmit）を処理する。
///
/// PendingInteractionRegistryから該当するインタラクションを検索し、
/// LoopEvent::InteractionResponseとしてイベントループに送信する。
pub(super) async fn handle_component_interaction(
    data: crate::gateway::ComponentInteractionData,
    registry: &opencrab_core::a2ui::PendingInteractionRegistry,
    renderer_http: Arc<serenity::http::Http>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
) {
    // Parse custom_id format: "interaction:{uuid}:{component_id}:{action_name}"
    let parts: Vec<&str> = data.custom_id.splitn(4, ':').collect();
    if parts.len() < 4 || parts[0] != "interaction" {
        warn!(custom_id = %data.custom_id, "Invalid A2UI custom_id format");
        return;
    }
    let interaction_id = parts[1].to_string();
    let component_id = parts[2].to_string();
    let action_name = parts[3].to_string();
    // serenityのインタラクション由来のguild_id（DMの場合は空）を保持。
    let guild_id = data.guild_id.clone();

    // Look up in registry, capture fields, then drop the ref
    let pending_data = {
        let pending_ref = registry.get(&interaction_id);
        match pending_ref {
            Some(ref pending) => {
                // Owner-only check.
                // オーナー未設定（空文字・空白のみ）なら誰も操作できない（#174）。
                // 以前は「空なら判定しない」＝誰でも操作可という fail-open だった。
                if !opencrab_core::owner::is_owner_id(&pending.owner_id, &data.user_id) {
                    debug!(
                        user_id = %data.user_id,
                        owner_id = %pending.owner_id,
                        "Non-owner tried to interact with owner-only UI"
                    );
                    return;
                }

                Some((
                    pending.session_id.clone(),
                    pending.agent_id.clone(),
                    // 保留状態はコアの `RenderTarget` を持つ（#156 S3）。Discord の
                    // チャンネル識別子は数値なので、移設前と同じフォールバック
                    // （`parse().unwrap_or(0)`）で数値化する。
                    pending.target.channel_id.parse::<u64>().unwrap_or(0),
                    pending.target.channel_id.clone(),
                    // 旧 `PendingInteraction.is_dm` は send_ui 時点で常に false が入って
                    // いた（送信時には判定できない）。移設後もその既定を保つ。
                    false,
                    pending.surface_id.clone(),
                    pending.rendered_message.clone(),
                    // resume の呼び出し元は**この UI を描いた run の caller**（#302）。
                    // クリックした本人からは導出しない: 上の owner-only ゲートで
                    // 押せるのはオーナーだけなので、応答者から導くと
                    // 「`Agent` のターンが描いた UI をオーナーが押す」＝昇格に
                    // なってしまう。
                    pending.caller.clone(),
                ))
            }
            None => {
                debug!(
                    interaction_id = %interaction_id,
                    "Interaction not found in registry (expired or already handled)"
                );
                None
            }
        }
    };

    let (
        session_id,
        agent_id,
        channel_id,
        channel_id_str,
        is_dm,
        surface_id,
        rendered_message,
        caller,
    ) = match pending_data {
        Some(d) => d,
        None => return,
    };

    // Handle ModalSubmit: extract field values and merge into context
    if data.interaction_kind == crate::gateway::InteractionKind::ModalSubmit {
        // Remove from registry
        let _ = registry.remove(&interaction_id);

        // Build context from modal values
        let mut context = serde_json::Map::new();
        if let Some(modal_values) = &data.modal_values {
            for (field_id, value) in modal_values {
                context.insert(field_id.clone(), serde_json::Value::String(value.clone()));
            }
        }

        let _ = event_tx.send(LoopEvent::InteractionResponse {
            interaction_id,
            session_id,
            agent_id,
            channel_id,
            channel_id_str,
            guild_id: guild_id.clone(),
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
            caller: caller.clone(),
        });
        return;
    }

    // Handle SelectMenu: merge selected_values into context
    if data.interaction_kind == crate::gateway::InteractionKind::SelectMenu {
        // Remove from registry
        let _ = registry.remove(&interaction_id);

        // Disable the select menu
        let renderer = crate::renderer::DiscordRenderer::new(renderer_http);
        let _ = renderer
            .update_on_response(
                &rendered_message,
                &opencrab_core::a2ui::UserActionResponse {
                    action_name: action_name.clone(),
                    context: None,
                    user_id: data.user_id.clone(),
                },
            )
            .await;

        // Build context with selected_values
        let mut context = serde_json::Map::new();
        if let Some(values) = &data.selected_values {
            context.insert(
                "selected_values".to_string(),
                serde_json::Value::Array(
                    values
                        .iter()
                        .map(|v| serde_json::Value::String(v.clone()))
                        .collect(),
                ),
            );
        }

        let _ = event_tx.send(LoopEvent::InteractionResponse {
            interaction_id,
            session_id,
            agent_id,
            channel_id,
            channel_id_str,
            guild_id: guild_id.clone(),
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
            caller: caller.clone(),
        });
        return;
    }

    // Handle Button: Form オープンは gateway の interaction_create で Modal 応答済み（ここには来ない）。

    // Remove from registry
    let _ = registry.remove(&interaction_id);

    // Disable buttons on the message
    let renderer = crate::renderer::DiscordRenderer::new(renderer_http);
    let _ = renderer
        .update_on_response(
            &rendered_message,
            &opencrab_core::a2ui::UserActionResponse {
                action_name: action_name.clone(),
                context: None,
                user_id: data.user_id.clone(),
            },
        )
        .await;

    // Send event to the loop
    let _ = event_tx.send(LoopEvent::InteractionResponse {
        interaction_id,
        session_id,
        agent_id,
        channel_id,
        channel_id_str,
        guild_id,
        response: opencrab_core::a2ui::A2uiUserAction {
            surface_id,
            component_id,
            action_name,
            context: None,
            responder_id: data.user_id,
        },
        is_dm,
        caller,
    });
}

/// A2UIインタラクション応答イベントを処理する。
///
/// SubtaskCompletedと同様のパターンで、応答情報をシステムプロンプトに含めて
/// エージェントを再呼び出しする。
///
/// `caller` は**この UI を描いた run の呼び出し元**（`PendingInteraction.caller` /
/// #298 / #302）。subtask 決着の resume（[`process_subtask_completed`]）とまったく
/// 同じ方針で、元のターンの呼び出し元を**引き継ぐだけ**。
///
/// `CallerIdentity::Agent` 固定にすると owner/trusted のツールが `policy_allows` で
/// 丸ごと消える（降格）。逆に応答者（`response.responder_id`）から導出すると昇格経路に
/// なる: `send_ui` の `channel_id` は自由引数なので、描画先チャンネルと resume 先
/// セッションは独立している。`Agent` のターンがオーナーの見るチャンネルへ UI を描き、
/// オーナーが押すとそのセッションが `Owner` で resume してしまう。
#[allow(clippy::too_many_arguments)]
pub(super) async fn process_interaction_response<T: AgentRunner>(
    interaction_id: String,
    session_id: String,
    agent_id: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    response: opencrab_core::a2ui::A2uiUserAction,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    caller: opencrab_actions::CallerIdentity,
) {
    info!(
        interaction_id = %interaction_id,
        action = %response.action_name,
        component = %response.component_id,
        "Processing A2UI interaction response"
    );

    // 1. Update DB
    {
        let response_json = serde_json::to_string(&response).ok();
        state.mark_interaction_status(
            &interaction_id,
            if response.action_name == "timeout" {
                "timeout"
            } else {
                "responded"
            },
            response_json.as_deref(),
            Some(&response.responder_id),
        );
    }

    // 2. Record in session_log
    {
        let log_content = format!(
            "[interaction_response] ユーザーがUIに応答しました。\nsurface_id: {}\ncomponent_id: {}\naction: {}\ncontext: {}\nresponder: {}",
            response.surface_id,
            response.component_id,
            response.action_name,
            response.context.as_ref().map(|c| c.to_string()).unwrap_or_default(),
            response.responder_id,
        );
        state.record_interaction_response(
            &agent_id,
            &session_id,
            &opencrab_actions::InteractionRecord {
                interaction_id: &interaction_id,
                surface_id: &response.surface_id,
                action_name: &response.action_name,
                component_id: &response.component_id,
                responder_id: &response.responder_id,
                content: &log_content,
            },
        );
    }

    // 3. Re-invoke agent (same pattern as SubtaskCompleted)
    // #352: 本ターンの caller で index を絞る（resume は元ターンの caller を引き継ぐ）。
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id, &caller);

    let context_str = response
        .context
        .as_ref()
        .map(|c| c.to_string())
        .unwrap_or_default();
    let system_prompt = format!(
        "{}\n\n{}\n[interaction_response: interaction_id={}, surface_id={}, action={}, component_id={}, context={}, responder={}]",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        interaction_id, response.surface_id,
        response.action_name, response.component_id, context_str, response.responder_id,
    );
    // #431: この経路も規則を揃える（subtask を起こしたターンには付けない）。
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
            .with_gateway_actions(gateway_actions)
            .with_subtask_starts(subtask_starts.clone())
            .with_reply_target(channel_id_str.clone())
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
                Ok(id) => id,
                Err(e) => {
                    error!("Interaction response Discord send failed: {e}");
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
                    context: Some(opencrab_actions::AgentReplyContext::InteractionResponse {
                        interaction_id: &interaction_id,
                    }),
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
