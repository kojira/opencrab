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

use opencrab_core::a2ui::UiRenderer;
use opencrab_gateway::DiscordGateway;
use opencrab_gateway::IncomingMessage;

/// 同一(channel, sender)のメッセージをまとめるまでの待機時間。
const DEBOUNCE_DELAY: Duration = Duration::from_secs(2);

use crate::gateway_actions::{CompletionRegistry, SubtaskCompletionFn};
use crate::AgentRunner;

/// メッセージループへの内部イベント。
pub enum LoopEvent {
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
    /// A2UIインタラクション応答（ボタンクリック or タイムアウト）。
    InteractionResponse {
        interaction_id: String,
        session_id: String,
        agent_id: String,
        channel_id: u64,
        channel_id_str: String,
        response: opencrab_core::a2ui::A2uiUserAction,
        is_dm: bool,
    },
}

/// Discordメッセージの受信→エージェント処理→応答送信のEvent-Drivenループ。
///
/// バックグラウンドタスクとして`tokio::spawn`から呼ばれることを想定。
/// Create the event channel pair for the discord loop.
///
/// Returns (sender, receiver). The sender should be cloned and given to
/// DiscordGatewayActions (via `with_a2ui`) so it can inject events.
pub fn create_event_channel() -> (
    mpsc::UnboundedSender<LoopEvent>,
    mpsc::UnboundedReceiver<LoopEvent>,
) {
    mpsc::unbounded_channel()
}

pub async fn run_discord_loop<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    completion_registry: CompletionRegistry,
    pending_registry: Option<crate::PendingInteractionRegistry>,
    event_channel: Option<(
        mpsc::UnboundedSender<LoopEvent>,
        mpsc::UnboundedReceiver<LoopEvent>,
    )>,
) {
    let (event_tx, mut event_rx) = match event_channel {
        Some((tx, rx)) => (tx, rx),
        None => mpsc::unbounded_channel::<LoopEvent>(),
    };

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

    // A2UIインタラクション受信タスク: gatewayのinteraction channelから受信して処理
    if let Some(ref registry) = pending_registry {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        let registry = registry.clone();
        let renderer_http = gateway.http().clone();
        tokio::spawn(async move {
            loop {
                match gw.recv_interaction().await {
                    Ok(data) => {
                        handle_component_interaction(
                            data,
                            &registry,
                            renderer_http.clone(),
                            tx.clone(),
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Discord interaction recv error: {e}");
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
        let next_deadline = debounce_buffers
            .values()
            .map(|(_, deadline)| *deadline)
            .min();

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
                    Some(LoopEvent::InteractionResponse {
                        interaction_id,
                        session_id,
                        agent_id,
                        channel_id,
                        channel_id_str,
                        response,
                        is_dm,
                    }) => {
                        process_interaction_response(
                            interaction_id,
                            session_id,
                            agent_id,
                            channel_id,
                            channel_id_str,
                            response,
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
                    let count = merged
                        .metadata
                        .get("debounce_count")
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
                let any_trusted = agent_ids
                    .iter()
                    .any(|aid| opencrab_db::queries::is_trusted_user(&conn, sender_id, aid));
                let any_registered = agent_ids
                    .iter()
                    .any(|aid| opencrab_db::queries::trusted_user_count(&conn, aid) > 0);
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
            let trust_info = agent_ids
                .iter()
                .find_map(|aid| opencrab_db::queries::get_trusted_user(&conn, sender_id, aid));
            drop(conn);
            match trust_info {
                Some(u) if u.permission == "co_agent" => {
                    opencrab_actions::CallerIdentity::CoAgent {
                        agent_id: sender_id.clone(),
                    }
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
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).ok();
        }

        let (base_prompt, agent_name) = state.build_agent_context(agent_id);
        let system_prompt = format!(
            "{}\n\n[Discord context: channel_id={}]",
            base_prompt, channel_id_str
        );
        let conversation_raw = match state.build_conversation_string(&session_id, agent_id, state.context_budget_tokens(agent_id)) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(session_id = %session_id, agent_id = %agent_id, "build_conversation_string failed: {e}");
                return;
            }
        };
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

            let completion_cb: SubtaskCompletionFn = Arc::new(
                move |subtask_id: String, result: String, exit_reason: String| {
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
                },
            );
            completion_registry.insert(session_id.clone(), completion_cb);
        }

        let on_response_text: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>> = {
            let state_db = state.db().clone();
            let gateway_for_cb = gateway.clone();
            let channel_id_str_for_cb = channel_id_str.clone();
            let is_dm_for_cb = is_dm;
            Some(std::sync::Arc::new(move |text: String| {
                if text.is_empty() || text.trim() == "NO_REPLY" {
                    return;
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
                let gateway_cb = gateway_for_cb.clone();
                tokio::spawn(async move {
                    if let Err(e) = gateway_cb.send_to_channel(channel_id, &text).await {
                        tracing::error!("on_response_text Discord send failed: {e}");
                    }
                });
            }))
        };

        // エージェント処理を直列で実行
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
                        created_at: None,
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
                    created_at: None,
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
    _result: String,
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
        state
            .db()
            .lock()
            .ok()
            .and_then(|conn| {
                opencrab_db::queries::get_session(&conn, &sub_session_id)
                    .ok()
                    .flatten()
            })
            .map(|s| {
                // theme is "Subtask: {task}", strip the prefix
                s.theme
                    .strip_prefix("Subtask: ")
                    .unwrap_or(&s.theme)
                    .to_string()
            })
            .unwrap_or_default()
    };

    let system_prompt = format!(
        "{}\n\n[Discord context: channel_id={}]\n[subtask_completed: subtask_id={}, task=\"{}\", exit_reason={}]",
        base_prompt, channel_id_str, subtask_id, task_description, exit_reason
    );
    let conversation_raw = match state.build_conversation_string(&session_id, &agent_id, state.context_budget_tokens(&agent_id)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(session_id = %session_id, agent_id = %agent_id, "build_conversation_string failed: {e}");
            return;
        }
    };
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
                        created_at: None,
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
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }
        _ => {}
    }
}

/// Discordコンポーネントインタラクション（ボタンクリック・セレクトメニュー・モーダルSubmit）を処理する。
///
/// PendingInteractionRegistryから該当するインタラクションを検索し、
/// LoopEvent::InteractionResponseとしてイベントループに送信する。
async fn handle_component_interaction(
    data: opencrab_gateway::ComponentInteractionData,
    registry: &crate::PendingInteractionRegistry,
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

    // Look up in registry, capture fields, then drop the ref
    let pending_data = {
        let pending_ref = registry.get(&interaction_id);
        match pending_ref {
            Some(ref pending) => {
                // Owner-only check
                if !pending.owner_discord_id.is_empty()
                    && data.user_id != pending.owner_discord_id
                {
                    debug!(
                        user_id = %data.user_id,
                        owner_id = %pending.owner_discord_id,
                        "Non-owner tried to interact with owner-only UI"
                    );
                    return;
                }

                Some((
                    pending.session_id.clone(),
                    pending.agent_id.clone(),
                    pending.channel_id,
                    pending.channel_id_str.clone(),
                    pending.is_dm,
                    pending.surface_id.clone(),
                    pending.rendered_message.clone(),
                    pending.form_data.as_ref().map(|fd| (
                        fd.modal_custom_id.clone(),
                        fd.title.clone(),
                        fd.action_rows.clone(),
                        fd.action.clone(),
                    )),
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

    let (session_id, agent_id, channel_id, channel_id_str, is_dm, surface_id, rendered_message, _form_data) =
        match pending_data {
            Some(d) => d,
            None => return,
        };

    // Handle ModalSubmit: extract field values and merge into context
    if data.interaction_kind == opencrab_gateway::InteractionKind::ModalSubmit {
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
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
        });
        return;
    }

    // Handle SelectMenu: merge selected_values into context
    if data.interaction_kind == opencrab_gateway::InteractionKind::SelectMenu {
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
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
        });
        return;
    }

    // Handle Button: check if this button should trigger a Modal (Form)
    // Note: For modal triggering, we DON'T remove from registry yet - the modal submit will do that
    // But we need to show the modal via a different mechanism. Since we already ACK'd
    // with UpdateMessage in the gateway, we can't show a modal here.
    // Instead, the Form trigger button approach needs the gateway to respond with Modal.
    // This is a limitation - for now, Form buttons need special handling in the gateway layer.
    // TODO: For full modal support, the gateway needs to detect form-trigger buttons
    //       and respond with CreateInteractionResponse::Modal instead of UpdateMessage.

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
        response: opencrab_core::a2ui::A2uiUserAction {
            surface_id,
            component_id,
            action_name,
            context: None,
            responder_id: data.user_id,
        },
        is_dm,
    });
}

/// A2UIインタラクション応答イベントを処理する。
///
/// SubtaskCompletedと同様のパターンで、応答情報をシステムプロンプトに含めて
/// エージェントを再呼び出しする。
async fn process_interaction_response<T: AgentRunner>(
    interaction_id: String,
    session_id: String,
    agent_id: String,
    channel_id: u64,
    channel_id_str: String,
    response: opencrab_core::a2ui::A2uiUserAction,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
) {
    info!(
        interaction_id = %interaction_id,
        action = %response.action_name,
        component = %response.component_id,
        "Processing A2UI interaction response"
    );

    // 1. Update DB
    {
        if let Ok(conn) = state.db().lock() {
            let response_json = serde_json::to_string(&response).ok();
            opencrab_db::queries::update_pending_interaction_status(
                &conn,
                &interaction_id,
                if response.action_name == "timeout" {
                    "timeout"
                } else {
                    "responded"
                },
                response_json.as_deref(),
                Some(&response.responder_id),
            )
            .ok();
        }
    }

    // 2. Record in session_log
    {
        if let Ok(conn) = state.db().lock() {
            let log_content = format!(
                "[interaction_response] ユーザーがUIに応答しました。\nsurface_id: {}\ncomponent_id: {}\naction: {}\ncontext: {}\nresponder: {}",
                response.surface_id,
                response.component_id,
                response.action_name,
                response.context.as_ref().map(|c| c.to_string()).unwrap_or_default(),
                response.responder_id,
            );
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
                log_type: "interaction_response".to_string(),
                content: log_content,
                speaker_id: Some("system".to_string()),
                turn_number: None,
                metadata_json: Some(
                    serde_json::json!({
                        "interaction_id": interaction_id,
                        "surface_id": response.surface_id,
                        "action_name": response.action_name,
                        "component_id": response.component_id,
                        "responder_id": response.responder_id,
                    })
                    .to_string(),
                ),
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).ok();
        }
    }

    // 3. Re-invoke agent (same pattern as SubtaskCompleted)
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id);

    let context_str = response
        .context
        .as_ref()
        .map(|c| c.to_string())
        .unwrap_or_default();
    let system_prompt = format!(
        "{}\n\n[Discord context: channel_id={}]\n[interaction_response: interaction_id={}, surface_id={}, action={}, component_id={}, context={}, responder={}]",
        base_prompt, channel_id_str, interaction_id, response.surface_id,
        response.action_name, response.component_id, context_str, response.responder_id,
    );
    let conversation_raw = match state.build_conversation_string(
        &session_id,
        &agent_id,
        state.context_budget_tokens(&agent_id),
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(session_id = %session_id, agent_id = %agent_id, "build_conversation_string failed: {e}");
            return;
        }
    };
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
                        created_at: None,
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
                error!("Interaction response Discord send failed: {e}");
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
                            "triggered_by": "interaction_response",
                            "interaction_id": interaction_id,
                        })
                        .to_string(),
                    ),
                    created_at: None,
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
                &conn,
                session_id,
                &metadata_json,
                &theme,
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
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
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
        opencrab_gateway::MessageContent::Image { url, .. } => (String::new(), vec![url.clone()]),
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
