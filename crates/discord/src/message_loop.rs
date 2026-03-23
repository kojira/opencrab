//! Discordゲートウェイのメッセージ処理ループ（Event-Driven v3）。
//!
//! v3の変更点:
//! - Event-Drivenモデル: IncomingMessageとSubtaskCompletedをmpscチャンネルで処理
//! - P0修正: should_send = !first_sent（engine.rsのon_first_response条件変更と組み合わせ）
//! - P1修正: 処理をtokio::spawnで非同期化、メインループをブロックしない
//! - P2修正: SubtaskCompleted callbackをLoopEvent送信に変更、イベントループで直列処理

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use opencrab_gateway::DiscordGateway;
use opencrab_gateway::IncomingMessage;

use crate::gateway_actions::{CompletionRegistry, SubtaskCompletionFn};
use crate::AgentRunner;

/// メッセージループへの内部イベント。
enum LoopEvent {
    /// Discordからの新規メッセージ。
    IncomingMessage(IncomingMessage),
    /// サブタスク完了通知（P2対策: tokio::spawnではなくイベントで直列処理）。
    SubtaskCompleted {
        session_id: String,
        agent_id: String,
        subtask_id: String,
        exit_reason: String,
        channel_id: u64,
        channel_id_str: String,
        is_dm: bool,
    },
}

/// Discordメッセージの受信→エージェント処理→応答送信のEvent-Drivenループ。
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
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<LoopEvent>();

    // Discord受信をイベントに変換するタスク（P1: メインループをブロックしない）
    {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            loop {
                match gw.recv().await {
                    Ok(msg) => {
                        let _ = tx.send(LoopEvent::IncomingMessage(msg));
                    }
                    Err(e) => {
                        error!("Discord recv error: {e}");
                        break;
                    }
                }
            }
        });
    }

    info!(
        agents = ?agent_ids,
        "Discord event loop v3 started"
    );

    // イベント処理ループ（直列）: P2のDB競合を構造的に解消
    loop {
        match event_rx.recv().await {
            Some(LoopEvent::IncomingMessage(msg)) => {
                process_incoming_message(
                    msg,
                    gateway.clone(),
                    state.clone(),
                    agent_ids.clone(),
                    gateway_actions.clone(),
                    owner_discord_id.clone(),
                    completion_registry.clone(),
                    event_tx.clone(),
                )
                .await;
            }
            Some(LoopEvent::SubtaskCompleted {
                session_id,
                agent_id,
                subtask_id,
                exit_reason,
                channel_id,
                channel_id_str,
                is_dm,
            }) => {
                process_subtask_completed(
                    session_id,
                    agent_id,
                    subtask_id,
                    exit_reason,
                    channel_id,
                    channel_id_str,
                    is_dm,
                    gateway.clone(),
                    state.clone(),
                    gateway_actions.clone(),
                )
                .await;
            }
            None => break,
        }
    }

    info!("Discord event loop v3 ended");
}

/// 受信メッセージを処理する。
///
/// バリデーション・セッション設定・エージェント処理のスポーンを行い、即座にリターン（P1）。
async fn process_incoming_message<T: AgentRunner>(
    incoming: IncomingMessage,
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    completion_registry: CompletionRegistry,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
) {
    let (text, image_urls) = extract_discord_content(&incoming.content);
    if text.is_empty() && image_urls.is_empty() {
        return;
    }

    let (guild_id, channel_id_str) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => return,
    };

    let channel_id: u64 = match channel_id_str.parse() {
        Ok(id) => id,
        Err(_) => return,
    };

    let is_dm = guild_id.is_empty();

    // DM whitelist check
    if is_dm {
        let sender_id = &incoming.sender.id;
        if !owner_discord_id.is_empty() && sender_id == &owner_discord_id {
            // allow
        } else {
            let allowed = {
                let conn = state.db().lock().unwrap();
                let any_trusted = agent_ids.iter().any(|aid| {
                    opencrab_db::queries::is_trusted_user(&conn, sender_id, aid)
                });
                let any_registered = agent_ids.iter().any(|aid| {
                    opencrab_db::queries::trusted_user_count(&conn, aid) > 0
                });
                if any_registered {
                    any_trusted
                } else {
                    owner_discord_id.is_empty() || sender_id == &owner_discord_id
                }
            };
            if !allowed {
                debug!(
                    sender = %incoming.sender.id,
                    "Ignoring DM from non-whitelisted user"
                );
                return;
            }
        }
    }

    // Channel whitelist check
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
            return;
        }
    }

    debug!(
        user = %incoming.sender.name,
        channel = channel_id,
        text = %text.chars().take(50).collect::<String>(),
        "Discord message received"
    );

    // タイピングインジケーター送信
    if let Err(e) = gateway.start_typing(channel_id).await {
        warn!("Failed to start typing indicator: {e}");
    }

    if !state.has_llm_providers() {
        debug!("No LLM providers configured, skipping agent response");
        return;
    }

    // 呼び出し元のアイデンティティを決定
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
                Some(u) if u.permission == "owner" => opencrab_actions::CallerIdentity::Owner,
                Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
                None => opencrab_actions::CallerIdentity::Agent,
            }
        }
    };

    let discord_message_id = incoming
        .metadata
        .get("discord_message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    for agent_id in &agent_ids {
        let session_id = format!("discord-{}-{}-{}", agent_id, guild_id, channel_id);
        ensure_discord_session(&state, &session_id, &[agent_id.clone()], &incoming);

        // ユーザーメッセージをDBにログ
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
                log_meta["image_urls"] = serde_json::json!(&image_urls);
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

        let (base_prompt, agent_name) = state.build_agent_context(agent_id);
        let system_prompt = format!(
            "{}\n\n[Discord context: channel_id={}]",
            base_prompt, channel_id_str
        );
        let conversation_raw = state.build_conversation_string(&session_id);
        let conversation = prepend_runtime_context_discord(
            &conversation_raw,
            "Discord conversation",
            &discord_message_id,
        );

        // サブタスク完了コールバック: LoopEventを送信（P2: tokio::spawnではなくイベントで直列処理）
        {
            let cb_event_tx = event_tx.clone();
            let agent_id_cb = agent_id.clone();
            let session_id_cb = session_id.clone();
            let channel_id_str_cb = channel_id_str.clone();

            let completion_cb: SubtaskCompletionFn =
                Arc::new(move |subtask_id: String, _result: String, exit_reason: String| {
                    let _ = cb_event_tx.send(LoopEvent::SubtaskCompleted {
                        session_id: session_id_cb.clone(),
                        agent_id: agent_id_cb.clone(),
                        subtask_id,
                        exit_reason,
                        channel_id,
                        channel_id_str: channel_id_str_cb.clone(),
                        is_dm,
                    });
                });
            completion_registry.insert(session_id.clone(), completion_cb);
        }

        // on_first_response: ツールなし応答のみ発火（P0: engine.rsの修正と組み合わせ）
        let first_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_sent_for_cb = first_sent.clone();
        let first_sent_for_handle = first_sent.clone();
        let first_response_speech: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let frs_for_cb = first_response_speech.clone();
        let frs_for_handle = first_response_speech.clone();
        let gateway_for_cb = gateway.clone();
        let channel_id_str_for_cb = channel_id_str.clone();
        let is_dm_for_cb = is_dm;

        let on_first_response: Option<Box<dyn FnOnce(String) + Send>> = {
            let state_db = state.db().clone();
            Some(Box::new(move |text: String| {
                if text.is_empty() || text.trim() == "NO_REPLY" {
                    return;
                }
                if let Ok(mut guard) = frs_for_cb.lock() {
                    *guard = Some(text.clone());
                }
                let writable = if is_dm_for_cb {
                    true
                } else {
                    state_db
                        .lock()
                        .map(|conn| {
                            opencrab_db::queries::is_channel_writable(&conn, &channel_id_str_for_cb)
                        })
                        .unwrap_or(false)
                };
                if !writable {
                    return;
                }
                first_sent_for_cb.store(true, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Err(e) = gateway_for_cb.send_to_channel(channel_id, &text).await {
                        tracing::error!("on_first_response Discord send failed: {e}");
                    }
                });
            }))
        };

        // エージェント処理をspawn（P1: メインループをブロックしない）
        let state_spawn = state.clone();
        let gateway_spawn = gateway.clone();
        let ga_spawn = gateway_actions.clone();
        let agent_id_spawn = agent_id.clone();
        let agent_name_spawn = agent_name.clone();
        let session_id_spawn = session_id.clone();
        let system_prompt_spawn = system_prompt.clone();
        let conversation_spawn = conversation.clone();
        let caller_spawn = caller.clone();
        let image_urls_spawn = image_urls.clone();
        let discord_message_id_spawn = discord_message_id.clone();
        let channel_id_str_spawn = channel_id_str.clone();

        tokio::spawn(async move {
            let result = state_spawn
                .run_agent_response(
                    &agent_id_spawn,
                    &agent_name_spawn,
                    &session_id_spawn,
                    &system_prompt_spawn,
                    &conversation_spawn,
                    "discord",
                    Some(ga_spawn),
                    caller_spawn,
                    &image_urls_spawn,
                    0,
                    if discord_message_id_spawn.is_empty() {
                        None
                    } else {
                        Some(discord_message_id_spawn)
                    },
                    on_first_response,
                )
                .await;

            handle_agent_response(
                result,
                &agent_id_spawn,
                &session_id_spawn,
                channel_id,
                &channel_id_str_spawn,
                is_dm,
                &gateway_spawn,
                &state_spawn,
                first_sent_for_handle,
                frs_for_handle,
            )
            .await;
        });
    }
}

/// エージェント応答結果を処理してDiscordに送信する。
async fn handle_agent_response<T: AgentRunner>(
    result: anyhow::Result<opencrab_core::EngineResult>,
    agent_id: &str,
    session_id: &str,
    channel_id: u64,
    channel_id_str: &str,
    is_dm: bool,
    gateway: &Arc<DiscordGateway>,
    state: &T,
    first_sent: Arc<std::sync::atomic::AtomicBool>,
    first_response_speech: Arc<std::sync::Mutex<Option<String>>>,
) {
    match result {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            // NO_REPLY は送信しない
            if engine_result.response.trim() == "NO_REPLY" {
                debug!(agent_id = %agent_id, "Agent returned NO_REPLY, skipping Discord send");
                // noreactと一緒に生成されたテキストをDBに保存
                if let Ok(guard) = first_response_speech.lock() {
                    if let Some(ref speech_text) = *guard {
                        if !speech_text.is_empty() {
                            let conn = state.db().lock().unwrap();
                            let log = opencrab_db::queries::SessionLogRow {
                                id: None,
                                agent_id: agent_id.to_string(),
                                session_id: session_id.to_string(),
                                log_type: "speech".to_string(),
                                content: speech_text.clone(),
                                speaker_id: Some(agent_id.to_string()),
                                turn_number: None,
                                metadata_json: Some(
                                    serde_json::json!({
                                        "source": "discord_response",
                                        "channel_id": channel_id_str,
                                        "via_noreact": true,
                                    })
                                    .to_string(),
                                ),
                            };
                            opencrab_db::queries::insert_session_log(&conn, &log).ok();
                        }
                    }
                }
                return;
            }

            // Writable check
            if !is_dm {
                let writable = {
                    let conn = state.db().lock().unwrap();
                    opencrab_db::queries::is_channel_writable(&conn, channel_id_str)
                };
                if !writable {
                    warn!(
                        agent_id = %agent_id,
                        channel = %channel_id_str,
                        "Skipping response to non-writable channel"
                    );
                    return;
                }
            }

            // P0修正: should_send = !first_sent
            // on_first_response はツールなし応答のみ発火（engine.rsで保証）。
            // first_sent=true → on_first_responseが既に送信済み → 再送しない。
            // first_sent=false → ツールあり応答 or no text → finalが唯一の送信。
            let should_send =
                !first_sent.load(std::sync::atomic::Ordering::SeqCst);
            if should_send {
                if let Err(e) = gateway
                    .send_to_channel(channel_id, &engine_result.response)
                    .await
                {
                    error!(agent_id = %agent_id, "Failed to send Discord reply: {e}");
                }
            }

            // DBにエージェント応答をログ
            let conn = state.db().lock().unwrap();
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: engine_result.response,
                speaker_id: Some(agent_id.to_string()),
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

/// サブタスク完了イベントを処理する（P2: イベントループで直列実行）。
async fn process_subtask_completed<T: AgentRunner>(
    session_id: String,
    agent_id: String,
    subtask_id: String,
    exit_reason: String,
    channel_id: u64,
    channel_id_str: String,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
) {
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id);
    let system_prompt = format!(
        "{}\n\n[Discord context: channel_id={}]\n[subtask_completed: subtask_id={}, exit_reason={}]",
        base_prompt, channel_id_str, subtask_id, exit_reason
    );
    let conversation_raw = state.build_conversation_string(&session_id);
    let conversation =
        prepend_runtime_context_discord(&conversation_raw, "Discord conversation", "");

    match state
        .run_agent_response(
            &agent_id,
            &agent_name,
            &session_id,
            &system_prompt,
            &conversation,
            "discord",
            Some(gateway_actions),
            opencrab_actions::CallerIdentity::Agent,
            &[],
            0,
            None,
            None,
        )
        .await
    {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            if engine_result.response.trim() == "NO_REPLY" {
                return;
            }
            if !is_dm {
                let writable = state
                    .db()
                    .lock()
                    .map(|conn| opencrab_db::queries::is_channel_writable(&conn, &channel_id_str))
                    .unwrap_or(false);
                if !writable {
                    return;
                }
            }
            if let Err(e) = gateway
                .send_to_channel(channel_id, &engine_result.response)
                .await
            {
                error!("Subtask completion Discord send failed: {e}");
            }
            // DBにログ
            if let Ok(conn) = state.db().lock() {
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
                            "triggered_by": "subtask_completed",
                        })
                        .to_string(),
                    ),
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }
        _ => {}
    }
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

/// Discord用: message_idを含む変動コンテキストを前置するヘルパー。
fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}

/// メッセージコンテンツからテキストと画像URLを抽出する。
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
