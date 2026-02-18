//! Discordゲートウェイのメッセージ処理ループ。
//!
//! Discordからメッセージを受信し、設定されたエージェントの応答を返す。
//! `discord` featureが有効な場合のみコンパイルされる。

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use opencrab_gateway::DiscordGateway;

use crate::process;
use crate::AppState;

/// Discordメッセージの受信→エージェント処理→応答送信のメインループ。
///
/// バックグラウンドタスクとして`tokio::spawn`から呼ばれることを想定。
pub async fn run_discord_loop(
    gateway: Arc<DiscordGateway>,
    state: AppState,
    agent_ids: Vec<String>,
    gateway_admin: Arc<dyn opencrab_actions::GatewayAdmin>,
    owner_discord_id: String,
) {
    info!(
        agents = ?agent_ids,
        "Discord message processing loop started"
    );

    loop {
        let incoming = match gateway.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Discord receive error: {e}");
                break;
            }
        };

        let text = match incoming.content.as_text() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => continue,
        };

        // Extract Discord channel ID for routing responses.
        let (guild_id, channel_id_str) = match &incoming.source {
            opencrab_gateway::MessageSource::Discord {
                guild_id,
                channel_id,
            } => (guild_id.clone(), channel_id.clone()),
            _ => continue,
        };

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };

        let is_dm = guild_id.is_empty();

        // DM owner check: DMの場合、設定されたオーナー以外からのメッセージは無視
        if is_dm && !owner_discord_id.is_empty() && incoming.sender.id != owner_discord_id {
            debug!(
                sender = %incoming.sender.id,
                owner = %owner_discord_id,
                "Ignoring DM from non-owner user"
            );
            continue;
        }

        // Channel readable check: DMはフィルタリング対象外
        if !is_dm {
            let readable = {
                let conn = state.db.lock().unwrap();
                opencrab_db::queries::is_channel_readable(&conn, &channel_id_str)
            };
            if !readable {
                debug!(
                    channel = %channel_id_str,
                    "Ignoring message from non-readable channel"
                );
                continue;
            }
        }

        debug!(
            user = %incoming.sender.name,
            channel = channel_id,
            text = %text.chars().take(50).collect::<String>(),
            "Discord message received"
        );

        // Session per Discord channel (auto-create if needed).
        let session_id = format!("discord-{}-{}", guild_id, channel_id);
        ensure_discord_session(&state, &session_id, &agent_ids);

        // Log the user's message.
        {
            let conn = state.db.lock().unwrap();
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: incoming.sender.id.clone(),
                session_id: session_id.clone(),
                log_type: "speech".to_string(),
                content: text.clone(),
                speaker_id: Some(incoming.sender.id.clone()),
                turn_number: None,
                metadata_json: Some(
                    serde_json::json!({
                        "source": "discord",
                        "channel_id": channel_id_str,
                        "user_name": incoming.sender.name,
                    })
                    .to_string(),
                ),
            };
            opencrab_db::queries::insert_session_log(&conn, &log).ok();
        }

        // Skip agent processing if no LLM providers are configured.
        if state.llm_router.provider_names().is_empty() {
            debug!("No LLM providers configured, skipping agent response");
            continue;
        }

        // Process with each configured agent.
        for agent_id in &agent_ids {
            let (system_prompt, agent_name) = {
                let conn = state.db.lock().unwrap();
                process::build_agent_context(&conn, agent_id, "Discord conversation")
            };

            let conversation = {
                let conn = state.db.lock().unwrap();
                process::build_conversation_string(&conn, &session_id)
            };

            let result = process::run_agent_response(
                &state,
                agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                Some(gateway_admin.clone()),
            )
            .await;

            match result {
                Ok(engine_result) if !engine_result.response.is_empty() => {
                    // Writable check: DMはフィルタリング対象外
                    if !is_dm {
                        let writable = {
                            let conn = state.db.lock().unwrap();
                            opencrab_db::queries::is_channel_writable(&conn, &channel_id_str)
                        };
                        if !writable {
                            warn!(
                                agent_id = %agent_id,
                                channel = %channel_id_str,
                                "Skipping response to non-writable channel"
                            );
                            continue;
                        }
                    }

                    // Send response to Discord.
                    if let Err(e) = gateway
                        .send_to_channel(channel_id, &engine_result.response)
                        .await
                    {
                        error!(agent_id = %agent_id, "Failed to send Discord reply: {e}");
                    }

                    // Log agent response to DB.
                    let conn = state.db.lock().unwrap();
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id.clone(),
                        session_id: session_id.clone(),
                        log_type: "speech".to_string(),
                        content: engine_result.response,
                        speaker_id: Some(agent_id.clone()),
                        turn_number: None,
                        metadata_json: Some(
                            serde_json::json!({
                                "source": "discord_response",
                                "channel_id": channel_id_str,
                                "tool_calls_made": engine_result.tool_calls_made,
                            })
                            .to_string(),
                        ),
                    };
                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                }
                Ok(_) => debug!(agent_id = %agent_id, "Agent produced empty response"),
                Err(e) => error!(agent_id = %agent_id, error = %e, "SkillEngine failed"),
            }
        }
    }

    info!("Discord message processing loop ended");
}

/// Discordチャンネル用のセッションが存在しなければ作成する。
fn ensure_discord_session(state: &AppState, session_id: &str, agent_ids: &[String]) {
    let conn = state.db.lock().unwrap();
    if opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }

    let session = opencrab_db::queries::SessionRow {
        id: session_id.to_string(),
        mode: "discord".to_string(),
        theme: "Discord conversation".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::to_string(agent_ids).unwrap_or_default(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
    };
    opencrab_db::queries::insert_session(&conn, &session).ok();
}
