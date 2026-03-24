//! Discordゲートウェイのメッセージ処理ループ（Event-Driven v3）。
//!
//! v3の変更点:
//! - Event-Drivenモデル: IncomingMessageとSubtaskCompletedをmpscチャンネルで処理
//! - P0修正: on_response_textコールバックでストリーミング応答を送信
//! - P1修正: 処理をtokio::spawnで非同期化、メインループをブロックしない
//! - P2修正: SubtaskCompleted callbackをLoopEvent送信に変更、イベントループで直列処理

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use opencrab_gateway::DiscordGateway;
use opencrab_gateway::IncomingMessage;

/// 同一(channel, sender)のメッセージをまとめるまでの待機時間。
const DEBOUNCE_DELAY: Duration = Duration::from_secs(2);

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
        result: String,
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
    let active_sessions: Arc<dashmap::DashMap<String, tokio::task::AbortHandle>> =
        Arc::new(dashmap::DashMap::new());

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
    // デバウンス: 同一(channel_id, sender_id)のメッセージを DEBOUNCE_DELAY 分まとめてから処理
    let mut debounce_buffers: HashMap<(String, String), (Vec<IncomingMessage>, Instant)> =
        HashMap::new();

    loop {
        // 次にフラッシュすべきバッファのデッドラインを計算
        let next_deadline = debounce_buffers.values().map(|(_, deadline)| *deadline).min();

        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(LoopEvent::IncomingMessage(msg)) => {
                        let key = debounce_key(&msg);
                        let entry = debounce_buffers
                            .entry(key)
                            .or_insert_with(|| (Vec::new(), Instant::now() + DEBOUNCE_DELAY));
                        entry.0.push(msg);
                        entry.1 = Instant::now() + DEBOUNCE_DELAY; // タイマーリセット
                    }
                    Some(LoopEvent::SubtaskCompleted {
                        session_id,
                        agent_id,
                        subtask_id,
                        result,
                        exit_reason,
                        channel_id,
                        channel_id_str,
                        is_dm,
                    }) => {
                        process_subtask_completed(
                            session_id,
                            agent_id,
                            subtask_id,
                            result,
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
            _ = tokio::time::sleep_until(next_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))), if !debounce_buffers.is_empty() => {
                // デッドラインを過ぎたバッファをフラッシュ
            }
        }

        // デバウンス期限が来たバッファをまとめて処理
        let now = Instant::now();
        let expired_keys: Vec<_> = debounce_buffers
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired_keys {
            if let Some((messages, _)) = debounce_buffers.remove(&key) {
                let merged = merge_incoming_messages(messages);
                if let Some(merged) = merged {
                    let count = merged.metadata.get("debounce_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1);
                    if count > 1 {
                        info!(
                            channel = %key.0,
                            sender = %key.1,
                            count = count,
                            "Debounced messages merged"
                        );
                    }
                    process_incoming_message(
                        merged,
                        gateway.clone(),
                        state.clone(),
                        agent_ids.clone(),
                        gateway_actions.clone(),
                        owner_discord_id.clone(),
                        completion_registry.clone(),
                        event_tx.clone(),
                        active_sessions.clone(),
                    )
                    .await;
                }
            }
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
    active_sessions: Arc<dashmap::DashMap<String, tokio::task::AbortHandle>>,
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
        let conversation_raw = state.build_conversation_string(&session_id, agent_id, state.context_budget_tokens());
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
                Arc::new(move |subtask_id: String, result: String, exit_reason: String| {
                    let _ = cb_event_tx.send(LoopEvent::SubtaskCompleted {
                        session_id: session_id_cb.clone(),
                        agent_id: agent_id_cb.clone(),
                        subtask_id,
                        result,
                        exit_reason,
                        channel_id,
                        channel_id_str: channel_id_str_cb.clone(),
                        is_dm,
                    });
                });
            completion_registry.insert(session_id.clone(), completion_cb);
        }

        let on_response_text: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>> = {
            let state_db = state.db().clone();
            let gateway_for_cb = gateway.clone();
            let channel_id_str_for_cb = channel_id_str.clone();
            let is_dm_for_cb = is_dm;
            Some(std::sync::Arc::new(move |text: String| {
                if text.is_empty() || text.trim() == "NO_REPLY" { return; }
                let writable = if is_dm_for_cb {
                    true
                } else {
                    state_db.lock().map(|conn| {
                        opencrab_db::queries::is_channel_writable(&conn, &channel_id_str_for_cb)
                    }).unwrap_or(false)
                };
                if !writable { return; }
                let gateway_cb = gateway_for_cb.clone();
                tokio::spawn(async move {
                    if let Err(e) = gateway_cb.send_to_channel(channel_id, &text).await {
                        tracing::error!("on_response_text Discord send failed: {e}");
                    }
                });
            }))
        };

        // 既存のセッションタスクがあればabort（二重実行防止）
        if let Some((_, old_handle)) = active_sessions.remove(&session_id) {
            warn!(session_id = %session_id, "Aborting existing session task due to new message interrupt");
            old_handle.abort();
        }

        // エージェント処理をspawn（P1: メインループをブロックしない）
        let state_spawn = state.clone();
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
        let active_sessions_spawn = active_sessions.clone();
        let session_id_for_cleanup = session_id.clone();

        let task_handle = tokio::spawn(async move {
            let prefix_msgs = {
                let conn = state_spawn.db().lock().unwrap();
                build_pending_tool_prefix_messages(&conn, &session_id_spawn, &agent_id_spawn)
            };
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
                    on_response_text,
                    prefix_msgs,
                )
                .await;

            handle_agent_response(
                result,
                &agent_id_spawn,
                &session_id_spawn,
                &channel_id_str_spawn,
                &state_spawn,
            )
            .await;

            active_sessions_spawn.remove(&session_id_for_cleanup);
        });
        active_sessions.insert(session_id.clone(), task_handle.abort_handle());
    }
}

/// エージェント応答結果を処理してDiscordに送信する。
async fn handle_agent_response<T: AgentRunner>(
    result: anyhow::Result<opencrab_core::EngineResult>,
    agent_id: &str,
    session_id: &str,
    channel_id_str: &str,
    state: &T,
) {
    match result {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            if engine_result.response.trim() == "NO_REPLY" {
                debug!(agent_id = %agent_id, "Agent returned NO_REPLY");
                if let Ok(conn) = state.db().lock() {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id.to_string(),
                        session_id: session_id.to_string(),
                        log_type: "speech".to_string(),
                        content: "NO_REPLY".to_string(),
                        speaker_id: Some(agent_id.to_string()),
                        turn_number: None,
                        metadata_json: Some(serde_json::json!({"no_reply": true}).to_string()),
                    };
                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                }
                return;
            }

            if let Ok(conn) = state.db().lock() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    log_type: "speech".to_string(),
                    content: engine_result.response.clone(),
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
    result: String,
    exit_reason: String,
    channel_id: u64,
    channel_id_str: String,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
) {
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id);

    // Get task description from subtask session
    let task_description = {
        let sub_session_id = format!("subtask-{}", subtask_id);
        state.db()
            .lock()
            .ok()
            .and_then(|conn| opencrab_db::queries::get_session(&conn, &sub_session_id).ok().flatten())
            .map(|s| {
                // theme is "Subtask: {task}", strip the prefix
                s.theme.strip_prefix("Subtask: ").unwrap_or(&s.theme).to_string()
            })
            .unwrap_or_default()
    };

    let system_prompt = format!(
        "{}\n\n[Discord context: channel_id={}]\n[subtask_completed: subtask_id={}, task=\"{}\", exit_reason={}]",
        base_prompt, channel_id_str, subtask_id, task_description, exit_reason
    );
    let conversation_raw = state.build_conversation_string(&session_id, &agent_id, state.context_budget_tokens());
    let conversation =
        prepend_runtime_context_discord(&conversation_raw, "Discord conversation", "");

    // subtask_idからtool_call_idを取得し、assistantメッセージ+tool_resultをその場で組み立てる
    let prefix_messages: Vec<opencrab_core::ChatMessage> = {
        let tool_call_id_opt = state.db()
            .lock()
            .ok()
            .and_then(|conn| {
                opencrab_db::queries::get_tool_call_id_for_subtask(&conn, &session_id, &subtask_id)
                    .ok()
                    .flatten()
            });

        match tool_call_id_opt {
            None => vec![],
            Some(tool_call_id) => {
                let tool_call_logs = state.db()
                    .lock()
                    .ok()
                    .map(|conn| {
                        opencrab_db::queries::list_tool_messages_for_session(&conn, &session_id)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();

                let assistant_msg_opt = tool_call_logs.iter().find(|log| {
                    log.metadata_json
                        .as_deref()
                        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                        .and_then(|v| {
                            v["tool_calls_json"]
                                .as_str()
                                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        })
                        .and_then(|arr| arr.as_array().map(|a| {
                            a.iter().any(|tc| tc["id"].as_str() == Some(&tool_call_id))
                        }))
                        .unwrap_or(false)
                });

                match assistant_msg_opt {
                    None => vec![],
                    Some(log) => {
                        let tool_calls: Vec<opencrab_core::ToolCall> = log.metadata_json
                            .as_deref()
                            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                            .and_then(|v| v["tool_calls_json"].as_str().map(|s| s.to_string()))
                            .and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or_default();

                        vec![
                            opencrab_core::ChatMessage {
                                role: "assistant".to_string(),
                                content: log.content.clone(),
                                tool_call_id: None,
                                tool_calls,
                                content_parts: vec![],
                                cache_control: None,
                            },
                            opencrab_core::ChatMessage {
                                role: "tool".to_string(),
                                content: result.clone(),
                                tool_call_id: Some(tool_call_id),
                                tool_calls: vec![],
                                content_parts: vec![],
                                cache_control: None,
                            },
                        ]
                    }
                }
            }
        }
    };

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
            prefix_messages,
        )
        .await
    {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            if engine_result.response.trim() == "NO_REPLY" {
                // NO_REPLY をsession_logに記録
                if let Ok(conn) = state.db().lock() {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id.to_string(),
                        session_id: session_id.clone(),
                        log_type: "speech".to_string(),
                        content: "NO_REPLY".to_string(),
                        speaker_id: Some(agent_id.to_string()),
                        turn_number: None,
                        metadata_json: Some(serde_json::json!({"no_reply": true}).to_string()),
                    };
                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                }
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

/// デバウンスキーを生成する: (channel_id, sender_id)。
fn debounce_key(msg: &IncomingMessage) -> (String, String) {
    let channel_id = match &msg.source {
        opencrab_gateway::MessageSource::Discord { channel_id, .. } => channel_id.clone(),
        _ => String::new(),
    };
    (channel_id, msg.sender.id.clone())
}

/// 複数のIncomingMessageを1つにマージする。
/// テキストは改行で結合、画像URLはすべて集約、メタデータは最後のメッセージを使用。
fn merge_incoming_messages(mut messages: Vec<IncomingMessage>) -> Option<IncomingMessage> {
    if messages.is_empty() {
        return None;
    }
    if messages.len() == 1 {
        return Some(messages.remove(0));
    }

    let count = messages.len();
    let mut texts = Vec::new();
    let mut images = Vec::new();

    for msg in &messages {
        let (text, img_urls) = extract_discord_content(&msg.content);
        if !text.is_empty() {
            texts.push(text);
        }
        images.extend(img_urls);
    }

    // 最後のメッセージをベースにする（最新のメタデータ・タイムスタンプ）
    let mut merged = messages.pop().unwrap();

    // コンテンツをマージ
    let merged_text = texts.join("\n");
    if images.is_empty() {
        merged.content = opencrab_gateway::MessageContent::Text(merged_text);
    } else {
        let mut parts: Vec<opencrab_gateway::ContentPart> = Vec::new();
        if !merged_text.is_empty() {
            parts.push(opencrab_gateway::ContentPart::Text(merged_text));
        }
        for url in images {
            parts.push(opencrab_gateway::ContentPart::Image { url, alt: None });
        }
        merged.content = opencrab_gateway::MessageContent::Multi(parts);
    }

    // デバウンスでまとめたことをメタデータに記録
    merged
        .metadata
        .insert("debounce_count".to_string(), serde_json::json!(count));

    Some(merged)
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

/// エージェントの最後のspeech以降にある、完了済みtool_call/tool_resultペアを
/// prefix messagesとして組み立てる。割り込みメッセージ時にLLMが前回のツール実行結果を
/// 認識できるようにする。
fn build_pending_tool_prefix_messages(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
) -> Vec<opencrab_core::ChatMessage> {
    let logs = match opencrab_db::queries::list_session_logs_by_session(conn, session_id) {
        Ok(logs) => logs,
        Err(_) => return vec![],
    };

    // agent_idのspeechログの最後のIDを見つける（NO_REPLYを除外）
    // NO_REPLYスピーチを除外: spawn_subtask→NO_REPLYの後に割り込みが来てもtool履歴が消えないようにする
    let last_meaningful_speech_id: i64 = logs
        .iter()
        .filter(|log| {
            if log.log_type != "speech" { return false; }
            if log.speaker_id.as_deref() != Some(agent_id) { return false; }
            // NO_REPLY speeches are excluded so their preceding tool_calls remain in prefix
            let is_no_reply = log.metadata_json
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| v["no_reply"].as_bool())
                .unwrap_or(false);
            !is_no_reply
        })
        .filter_map(|log| log.id)
        .last()
        .unwrap_or(0);

    // last_meaningful_speech_id以降のtool_call / tool_resultログを抽出
    let after_speech: Vec<_> = logs
        .iter()
        .filter(|log| log.id.unwrap_or(0) > last_meaningful_speech_id)
        .filter(|log| log.log_type == "tool_call" || log.log_type == "tool_result")
        .collect();

    // tool_resultログから tool_call_id -> content のHashMapを構築
    let mut result_map: HashMap<String, String> = HashMap::new();
    for log in &after_speech {
        if log.log_type == "tool_result" {
            if let Some(tc_id) = log
                .metadata_json
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|v| v["tool_call_id"].as_str().map(|s| s.to_string()))
            {
                result_map.insert(tc_id, log.content.clone());
            }
        }
    }

    // tool_callログを処理してprefix messagesを組み立てる
    let mut prefix = Vec::new();
    for log in &after_speech {
        if log.log_type != "tool_call" {
            continue;
        }

        let tool_calls: Vec<opencrab_core::ToolCall> = log
            .metadata_json
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| v["tool_calls_json"].as_str().map(|s| s.to_string()))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        if tool_calls.is_empty() {
            continue;
        }

        // assistant ChatMessage（result_mapにあるかどうかに関わらず常に含める）
        prefix.push(opencrab_core::ChatMessage {
            role: "assistant".to_string(),
            content: log.content.clone(),
            tool_call_id: None,
            tool_calls: tool_calls.clone(),
            content_parts: vec![],
            cache_control: None,
        });

        // 各tool_callに対応するtool result ChatMessage
        // resultがない場合は合成メッセージ（中断）を使用
        for tc in &tool_calls {
            let result_content = result_map
                .get(&tc.id)
                .cloned()
                .unwrap_or_else(|| {
                    "[Tool execution was interrupted by a new message. Please retry if needed.]"
                        .to_string()
                });
            prefix.push(opencrab_core::ChatMessage {
                role: "tool".to_string(),
                content: result_content,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: vec![],
                content_parts: vec![],
                cache_control: None,
            });
        }
    }

    prefix
}
