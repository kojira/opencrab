//! Discordゲートウェイのメッセージ処理ループ。
//!
//! Discordからメッセージを受信し、設定されたエージェントの応答を返す。

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use opencrab_gateway::DiscordGateway;
use opencrab_gateway::IncomingMessage;

use crate::gateway_actions::{CompletionRegistry, SubtaskCompletionFn};
use crate::AgentRunner;

/// Discordメッセージの受信→エージェント処理→応答送信のメインループ。
///
/// バックグラウンドタスクとして`tokio::spawn`から呼ばれることを想定。
pub async fn run_discord_loop<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    completion_registry: CompletionRegistry,
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

        let (text, image_urls) = extract_discord_content(&incoming.content);
        if text.is_empty() && image_urls.is_empty() {
            continue;
        }

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

        // DM whitelist check: ownerは常に許可。ホワイトリストが空なら既存動作（ownerのみ）。
        // ホワイトリストに登録があれば、登録ユーザーのみ許可。
        if is_dm {
            let sender_id = &incoming.sender.id;
            // ownerは常に許可
            if !owner_discord_id.is_empty() && sender_id == &owner_discord_id {
                // allow
            } else {
                // ホワイトリストを確認（最初にマッチしたagentのDBを使う）
                let allowed = {
                    let conn = state.db().lock().unwrap();
                    // 全agent_idのホワイトリストをチェック
                    let any_trusted = agent_ids.iter().any(|aid| {
                        opencrab_db::queries::is_trusted_user(&conn, sender_id, aid)
                    });
                    // ホワイトリストが空かどうかも確認（空なら既存動作=ownerのみ）
                    let any_registered = agent_ids.iter().any(|aid| {
                        opencrab_db::queries::trusted_user_count(&conn, aid) > 0
                    });
                    if any_registered {
                        any_trusted
                    } else {
                        // ホワイトリスト未設定: ownerのみ許可（ownerでなければ拒否）
                        owner_discord_id.is_empty() || sender_id == &owner_discord_id
                    }
                };
                if !allowed {
                    debug!(
                        sender = %incoming.sender.id,
                        "Ignoring DM from non-whitelisted user"
                    );
                    continue;
                }
            }
        }

        // Channel whitelist check: DMはフィルタリング対象外
        if !is_dm {
            let whitelisted = {
                let conn = state.db().lock().unwrap();
                opencrab_db::queries::is_channel_whitelisted(&conn, &channel_id_str)
            };
            if !whitelisted {
                debug!(
                    channel = %channel_id_str,
                    "Ignoring message from non-whitelisted channel"
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

        // タイピングインジケーターを送信（エンジン処理開始前）
        if let Err(e) = gateway.start_typing(channel_id).await {
            warn!("Failed to start typing indicator: {e}");
        }

        // Skip agent processing if no LLM providers are configured.
        if !state.has_llm_providers() {
            debug!("No LLM providers configured, skipping agent response");
            continue;
        }

        // Determine caller identity for permission checks.
        let caller = {
            let sender_id = &incoming.sender.id;
            if !owner_discord_id.is_empty() && sender_id == &owner_discord_id {
                opencrab_actions::CallerIdentity::Owner
            } else {
                let conn = state.db().lock().unwrap();
                let trust_info = agent_ids.iter().find_map(|aid| {
                    opencrab_db::queries::get_trusted_user(&conn, sender_id, aid)
                });
                drop(conn);
                match trust_info {
                    Some(u) if u.permission == "co_agent" => {
                        opencrab_actions::CallerIdentity::CoAgent { agent_id: sender_id.clone() }
                    }
                    Some(u) if u.permission == "owner" => {
                        opencrab_actions::CallerIdentity::Owner
                    }
                    Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
                    None => opencrab_actions::CallerIdentity::Agent,
                }
            }
        };

        // Extract discord_message_id for context injection.
        let discord_message_id = incoming
            .metadata
            .get("discord_message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Process with each configured agent.
        for agent_id in &agent_ids {
            // BUG FIX: session_id にagent_idを含め、エージェントごとに独立した会話履歴を持たせる。
            // 旧形式 "discord-{guild}-{channel}" では複数Botが同じチャンネルにいると会話ログが混在していた。
            let session_id = format!("discord-{}-{}-{}", agent_id, guild_id, channel_id);
            ensure_discord_session(&state, &session_id, &[agent_id.clone()], &incoming);

            // Log the user's message (per-agent session).
            {
                let conn = state.db().lock().unwrap();
                let mut log_meta = serde_json::json!({
                    "source": "discord",
                    "channel_id": channel_id_str,
                    "user_name": incoming.sender.name,
                });
                if let Some(ref avatar_url) = incoming.sender.avatar_url {
                    log_meta["user_avatar_url"] = serde_json::json!(avatar_url);
                }
                if !image_urls.is_empty() {
                    log_meta["image_urls"] = serde_json::json!(image_urls);
                }
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: incoming.sender.id.clone(),
                    session_id: session_id.clone(),
                    log_type: "speech".to_string(),
                    content: text.clone(),
                    speaker_id: Some(incoming.sender.id.clone()),
                    turn_number: None,
                    metadata_json: Some(log_meta.to_string()),
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }

            let (base_prompt, agent_name) = state.build_agent_context(agent_id, "Discord conversation");

            // Bug 1 fix: inject Discord channel/message context so LLM can use reactions.
            let system_prompt = format!(
                "{}\n\n[Discord context: channel_id={}, message_id={}]",
                base_prompt, channel_id_str, discord_message_id
            );

            let conversation = state.build_conversation_string(&session_id);

            // Track whether on_first_response already sent a message to Discord.
            let first_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let first_sent_clone = first_sent.clone();
            let first_response_speech: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
            let frs_for_cb = first_response_speech.clone();
            let gateway_for_cb = gateway.clone();
            let channel_id_for_cb = channel_id;
            let is_dm_for_cb = is_dm;
            let channel_id_str_for_cb = channel_id_str.clone();

            let on_first_response: Option<Box<dyn FnOnce(String) + Send>> = {
                let state_db = state.db().clone();
                Some(Box::new(move |text: String| {
                    if text.is_empty() {
                        return;
                    }
                    // NO_REPLY は送信しない
                    if text.trim() == "NO_REPLY" {
                        return;
                    }
                    // Save the first response text (for logging when engine returns NO_REPLY)
                    if let Ok(mut guard) = frs_for_cb.lock() {
                        *guard = Some(text.clone());
                    }
                    // Only send if the channel is writable (or DM)
                    let writable = if is_dm_for_cb {
                        true
                    } else {
                        state_db.lock()
                            .map(|conn| opencrab_db::queries::is_channel_writable(&conn, &channel_id_str_for_cb))
                            .unwrap_or(false)
                    };
                    if !writable {
                        return;
                    }
                    first_sent_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    // Spawn async send in a new tokio task
                    tokio::spawn(async move {
                        if let Err(e) = gateway_for_cb.send_to_channel(channel_id_for_cb, &text).await {
                            tracing::error!("on_first_response Discord send failed: {e}");
                        }
                    });
                }))
            };

            // Register subtask completion callback for this session.
            {
                let gw_cb = gateway.clone();
                let state_cb = state.clone();
                let ga_cb = gateway_actions.clone();
                let agent_id_cb = agent_id.clone();
                let session_id_cb = session_id.clone();
                let channel_id_cb = channel_id;
                let is_dm_cb = is_dm;
                let channel_id_str_cb = channel_id_str.clone();
                let caller_cb = caller.clone();
                let db_cb = state.db().clone();

                let completion_cb: SubtaskCompletionFn = Arc::new(move |subtask_id: String, _result: String, exit_reason: String| {
                    let gw = gw_cb.clone();
                    let state = state_cb.clone();
                    let ga = ga_cb.clone();
                    let agent_id = agent_id_cb.clone();
                    let session_id = session_id_cb.clone();
                    let channel_id = channel_id_cb;
                    let is_dm = is_dm_cb;
                    let channel_id_str = channel_id_str_cb.clone();
                    let caller = caller_cb.clone();
                    let db = db_cb.clone();
                    let exit_reason_clone = exit_reason.clone();

                    tokio::spawn(async move {
                        let (base_prompt, agent_name) = state.build_agent_context(&agent_id, "Discord conversation");
                        let system_prompt = format!(
                            "{}\n\n[Discord context: channel_id={}]\n[subtask_completed: subtask_id={}, exit_reason={}]",
                            base_prompt, channel_id_str, subtask_id, exit_reason_clone
                        );
                        let conversation = state.build_conversation_string(&session_id);

                        match state.run_agent_response(
                            &agent_id,
                            &agent_name,
                            &session_id,
                            &system_prompt,
                            &conversation,
                            "discord",
                            Some(ga),
                            caller,
                            &[],
                            0,
                            None,
                        ).await {
                            Ok(engine_result) if !engine_result.response.is_empty() => {
                                // NO_REPLY は送信しない
                                if engine_result.response.trim() == "NO_REPLY" {
                                    return;
                                }
                                // Writable check
                                if !is_dm {
                                    let writable = db.lock()
                                        .map(|conn| opencrab_db::queries::is_channel_writable(&conn, &channel_id_str))
                                        .unwrap_or(false);
                                    if !writable { return; }
                                }
                                if let Err(e) = gw.send_to_channel(channel_id, &engine_result.response).await {
                                    tracing::error!("Subtask completion Discord send failed: {e}");
                                }
                                // Log to DB
                                if let Ok(conn) = db.lock() {
                                    let log = opencrab_db::queries::SessionLogRow {
                                        id: None,
                                        agent_id: agent_id.clone(),
                                        session_id: session_id.clone(),
                                        log_type: "speech".to_string(),
                                        content: engine_result.response,
                                        speaker_id: Some(agent_id.clone()),
                                        turn_number: None,
                                        metadata_json: Some(serde_json::json!({
                                            "source": "discord_response",
                                            "channel_id": channel_id_str,
                                            "triggered_by": "subtask_completed",
                                        }).to_string()),
                                    };
                                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                                }
                            }
                            _ => {}
                        }
                    });
                });

                completion_registry.insert(session_id.clone(), completion_cb);
            }

            let result = state
                .run_agent_response(
                    agent_id,
                    &agent_name,
                    &session_id,
                    &system_prompt,
                    &conversation,
                    "discord",
                    Some(gateway_actions.clone()),
                    caller.clone(),
                    &image_urls,
                    0,  // depth = 0 for main engine
                    on_first_response,
                )
                .await;

            match result {
                Ok(engine_result) if !engine_result.response.is_empty() => {
                    // NO_REPLY は送信しない
                    if engine_result.response.trim() == "NO_REPLY" {
                        debug!(agent_id = %agent_id, "Agent returned NO_REPLY, skipping Discord send");
                        // noreactと一緒に生成されたテキストをmemory_sessionsに保存
                        if let Ok(guard) = first_response_speech.lock() {
                            if let Some(ref speech_text) = *guard {
                                if !speech_text.is_empty() {
                                    let conn = state.db().lock().unwrap();
                                    let log = opencrab_db::queries::SessionLogRow {
                                        id: None,
                                        agent_id: agent_id.clone(),
                                        session_id: session_id.clone(),
                                        log_type: "speech".to_string(),
                                        content: speech_text.clone(),
                                        speaker_id: Some(agent_id.clone()),
                                        turn_number: None,
                                        metadata_json: Some(
                                            serde_json::json!({
                                                "source": "discord_response",
                                                "channel_id": channel_id_str,
                                                "tool_calls_made": engine_result.tool_calls_made,
                                                "via_noreact": true,
                                            })
                                            .to_string(),
                                        ),
                                    };
                                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                                }
                            }
                        }
                        continue;
                    }
                    // Writable check: DMはフィルタリング対象外
                    if !is_dm {
                        let writable = {
                            let conn = state.db().lock().unwrap();
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

                    // Send final response if:
                    // - on_first_response didn't already send, OR
                    // - tool calls were made (final response after tools differs from first streamed chunk)
                    let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst)
                        || engine_result.tool_calls_made > 0;
                    if should_send {
                        if let Err(e) = gateway
                            .send_to_channel(channel_id, &engine_result.response)
                            .await
                        {
                            error!(agent_id = %agent_id, "Failed to send Discord reply: {e}");
                        }
                    }

                    // Log agent response to DB.
                    let conn = state.db().lock().unwrap();
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

/// IncomingMessage からセッション用のリッチメタデータとテーマを構築する。
fn build_discord_session_metadata(incoming: &IncomingMessage) -> (String, String) {
    let (guild_id, channel_id) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => (String::new(), String::new()),
    };

    let is_dm = guild_id.is_empty();

    if is_dm {
        let dm_user_name = incoming.sender.name.clone();
        let theme = format!("DM with {}", dm_user_name);
        let mut meta = serde_json::json!({
            "source": "discord",
            "is_dm": true,
            "channel_id": channel_id,
            "dm_user_name": dm_user_name,
            "dm_user_id": incoming.sender.id,
        });
        if let Some(ref avatar_url) = incoming.sender.avatar_url {
            meta["dm_user_avatar_url"] = serde_json::json!(avatar_url);
        }
        (theme, meta.to_string())
    } else {
        let guild_name = incoming
            .metadata
            .get("guild_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let guild_icon_url = incoming
            .metadata
            .get("guild_icon_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel_name = incoming
            .metadata
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let theme = if !channel_name.is_empty() && !guild_name.is_empty() {
            format!("#{} in {}", channel_name, guild_name)
        } else {
            "Discord conversation".to_string()
        };

        let meta = serde_json::json!({
            "source": "discord",
            "is_dm": false,
            "guild_id": guild_id,
            "guild_name": guild_name,
            "guild_icon_url": guild_icon_url,
            "channel_id": channel_id,
            "channel_name": channel_name,
        });
        (theme, meta.to_string())
    }
}

/// Discordチャンネル用のセッションが存在しなければ作成する。
/// 既存セッションで metadata_json が未設定の場合は更新する。
fn ensure_discord_session<T: AgentRunner>(
    state: &T,
    session_id: &str,
    agent_ids: &[String],
    incoming: &IncomingMessage,
) {
    let conn = state.db().lock().unwrap();

    if let Some(existing) = opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
    {
        // 既存セッションで metadata_json が未設定なら更新
        if existing.metadata_json.is_none() {
            let (theme, metadata_json) = build_discord_session_metadata(incoming);
            opencrab_db::queries::update_session_metadata(
                &conn, session_id, &metadata_json, &theme,
            )
            .ok();
        }
        return;
    }

    let (theme, metadata_json) = build_discord_session_metadata(incoming);

    let session = opencrab_db::queries::SessionRow {
        id: session_id.to_string(),
        mode: "discord".to_string(),
        theme,
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::to_string(agent_ids).unwrap_or_default(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: Some(metadata_json),
    };
    opencrab_db::queries::insert_session(&conn, &session).ok();
}

/// メッセージコンテンツからテキストと画像URLを抽出する
fn extract_discord_content(content: &opencrab_gateway::MessageContent) -> (String, Vec<String>) {
    match content {
        opencrab_gateway::MessageContent::Text(t) => (t.clone(), vec![]),
        opencrab_gateway::MessageContent::Image { url, .. } => {
            (String::new(), vec![url.clone()])
        }
        opencrab_gateway::MessageContent::Multi(parts) => {
            let mut texts = Vec::new();
            let mut urls = Vec::new();
            for part in parts {
                match part {
                    opencrab_gateway::ContentPart::Text(t) => texts.push(t.clone()),
                    opencrab_gateway::ContentPart::Image { url, .. } => urls.push(url.clone()),
                }
            }
            (texts.join(" "), urls)
        }
    }
}
