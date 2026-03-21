use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_server::{config, create_router, AppState};
use opencrab_core::heartbeat::{HeartbeatCallback, HeartbeatConfig, HeartbeatDecision, heartbeat_loop};
use tokio::sync::watch;

fn build_heartbeat_prompt(
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> String {
    let soul_text = opencrab_db::queries::get_soul(conn, agent_id)
        .ok()
        .flatten()
        .map(|s| format!("名前: {}\nペルソナ: {}", s.persona_name, s.personality_json))
        .unwrap_or_else(|| "AIエージェント".to_string());

    let memories = opencrab_db::queries::get_curated_memories(conn, agent_id, "")
        .unwrap_or_default()
        .iter()
        .map(|m| format!("[{}] {}", m.category, m.content))
        .collect::<Vec<_>>()
        .join("\n");

    let last_speak_info = conn.query_row(
        "SELECT created_at FROM heartbeat_log WHERE agent_id = ?1 AND decision = 'speak' ORDER BY created_at DESC LIMIT 1",
        rusqlite::params![agent_id],
        |row| row.get::<_, String>(0),
    ).ok()
     .map(|t| format!("最後に発言した時刻: {}", t))
     .unwrap_or_else(|| "まだ一度も発言していない".to_string());

    format!(
        "あなたはAIエージェントです。以下のSoul（性格）と記憶、最後の発言情報を踏まえ、今この瞬間に何をすべきか判断してください。\n\n## Soul（性格・ペルソナ）\n{}\n\n## 最近の記憶\n{}\n\n## 発言履歴\n{}\n\n## 判断ルール\n以下の3つのいずれかを選んでください：\n- `SPEAK: <メッセージ>` — Discordで何か発言する（自然な独り言、気づき、感想など。日本語で）\n- `LEARN` — 内省・自己振り返りを行う（発言はしない）\n- `IDLE` — 何もしない\n\n## 重要\n- 発言は最低でも30分に1回以下が望ましい。最後の発言から時間が経っていない場合はIDLEまたはLEARNを選ぶ\n- SPEAKの場合は自然で短いメッセージにする（1〜2文程度）\n- 回答は必ず上記3形式のいずれか1行のみ。余計な説明は不要",
        soul_text,
        if memories.is_empty() { "記憶なし".to_string() } else { memories },
        last_speak_info,
    )
}

async fn evaluate_heartbeat_action_llm(
    prompt: String,
    llm_router: Arc<opencrab_llm::LlmRouter>,
    default_model: &str,
) -> HeartbeatDecision {
    use opencrab_llm::{ChatRequest, Message, MessageContent, Role};

    let request = ChatRequest {
        model: default_model.to_string(),
        messages: vec![
            Message {
                role: Role::User,
                content: Some(MessageContent::Text(prompt)),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            }
        ],
        max_tokens: Some(200),
        temperature: Some(0.7),
        stream: Some(false),
        functions: None,
        function_call: None,
        stop: None,
        metadata: Default::default(),
    };

    match llm_router.chat_completion(request).await {
        Ok(response) => {
            let text = response.choices.into_iter()
                .next()
                .and_then(|c| match c.message.content {
                    Some(MessageContent::Text(t)) => Some(t),
                    _ => None,
                })
                .unwrap_or_default()
                .trim()
                .to_string();

            if text.starts_with("SPEAK:") {
                let content = text.trim_start_matches("SPEAK:").trim().to_string();
                if content.is_empty() {
                    HeartbeatDecision::Idle
                } else {
                    HeartbeatDecision::Speak(content)
                }
            } else if text.starts_with("LEARN") {
                HeartbeatDecision::Learn
            } else {
                HeartbeatDecision::Idle
            }
        }
        Err(e) => {
            tracing::warn!("Heartbeat LLM call failed: {e}");
            HeartbeatDecision::Idle
        }
    }
}

/// config名またはUUIDのagent_idを、DBのUUIDに解決する。
/// "crab"のような名前が渡された場合、find_agentsで検索してUUIDを返す。
fn resolve_agent_id(conn: &rusqlite::Connection, agent_id: &str) -> String {
    // まず直接lookupを試みる
    if let Ok(Some(_)) = opencrab_db::queries::get_identity(conn, agent_id) {
        return agent_id.to_string();
    }
    // 名前で検索
    if let Ok(agents) = opencrab_db::queries::find_agents(conn, agent_id) {
        if let Some((uuid, _name)) = agents.first() {
            tracing::info!(config_id = %agent_id, uuid = %uuid, "Resolved agent_id config name to UUID");
            return uuid.clone();
        }
    }
    // シングルエージェントフォールバック: DBに登録済みのエージェントが1つだけの場合はそれを使う
    // (config名"crab"などがDBの名前と一致しない場合の対応)
    if let Ok(all_agents) = opencrab_db::queries::find_agents(conn, "") {
        if all_agents.len() == 1 {
            let (uuid, name) = &all_agents[0];
            tracing::info!(
                config_id = %agent_id,
                uuid = %uuid,
                name = %name,
                "Resolved agent_id to only registered agent (single-agent fallback)"
            );
            return uuid.clone();
        }
    }
    tracing::warn!(agent_id = %agent_id, "Could not resolve agent_id to UUID, using as-is");
    agent_id.to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("opencrab=info".parse()?))
        .init();

    tracing::info!("Starting OpenCrab server...");

    // Load config from TOML (with env var expansion)
    let cfg = config::load_config("config/default.toml")?;

    // DB初期化
    let conn = opencrab_db::init_connection(&cfg.database.path)?;

    // Build LLM router from config
    let llm_router = config::build_llm_router(&cfg.llm)?;

    let default_model = format!(
        "{}:{}",
        cfg.llm.default_provider, cfg.llm.default_model
    );

    #[allow(unused_mut)]
    let mut state = AppState {
        db: Arc::new(Mutex::new(conn)),
        llm_router: Arc::new(llm_router),
        workspace_base: cfg.agent.workspace_path.clone(),
        tools_config: Arc::new(std::sync::RwLock::new(cfg.tools.clone())),
        default_model,
        #[cfg(feature = "discord")]
        discord_manager: None,
    };

    #[cfg(feature = "discord")]
    let heartbeat_discord_http: Arc<Mutex<Option<Arc<serenity::http::Http>>>> =
        Arc::new(Mutex::new(None));
    #[cfg(not(feature = "discord"))]
    let heartbeat_discord_http: Arc<Mutex<Option<()>>> =
        Arc::new(Mutex::new(None));
    let heartbeat_channel_id_arc: Arc<Mutex<Option<u64>>> =
        Arc::new(Mutex::new(None));

    // Start Discord gateway if configured and feature is enabled.
    #[cfg(feature = "discord")]
    {
        let discord_cfg = &cfg.gateway.discord;

        // Fallback: config-based shared gateway (existing behavior).
        if discord_cfg.enabled && !discord_cfg.token.is_empty() {
            tracing::info!("Starting Discord gateway (config-based fallback)...");

            // Validate agent IDs against the database
            let valid_agent_ids: Vec<String> = {
                let conn = state.db.lock().unwrap();
                discord_cfg
                    .agent_ids
                    .iter()
                    .filter(|agent_id| {
                        match opencrab_db::queries::get_identity(&conn, agent_id) {
                            Ok(Some(_)) => true,
                            _ => {
                                tracing::warn!("Agent '{}' not found in database, skipping", agent_id);
                                false
                            }
                        }
                    })
                    .cloned()
                    .collect()
            };

            if valid_agent_ids.is_empty() {
                tracing::error!("No valid agents found for Discord gateway, not starting");
            } else {
                let gateway = Arc::new(opencrab_gateway::DiscordGateway::new(&discord_cfg.token));
                gateway.start().await?;

                let first_agent_id = valid_agent_ids.first().cloned().unwrap_or_default();
                let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
                    opencrab_discord::DiscordGatewayActions::new(
                        gateway.http().clone(),
                        state.db.clone(),
                        first_agent_id,
                    ),
                );

                *heartbeat_discord_http.lock().unwrap() = Some(gateway.http().clone());

                let discord_state = state.clone();
                let owner_discord_id = discord_cfg.owner_discord_id.clone();
                tokio::spawn(async move {
                    opencrab_discord::run_discord_loop(
                        gateway,
                        discord_state,
                        valid_agent_ids,
                        gateway_actions,
                        owner_discord_id,
                    )
                    .await;
                });
                if let Some(ch_id) = discord_cfg.heartbeat_channel_id {
                    *heartbeat_channel_id_arc.lock().unwrap() = Some(ch_id);
                }

                tracing::info!(
                    agents = ?discord_cfg.agent_ids,
                    owner = %discord_cfg.owner_discord_id,
                    "Discord gateway started (config-based)"
                );
            }
        }

        // Per-agent Discord gateway manager.
        let manager = opencrab_discord::DiscordGatewayManager::new(state.clone());
        manager.restore_from_db().await;
        state.discord_manager = Some(Arc::new(manager));

        tracing::info!("Per-agent Discord gateway manager initialized");
    }

    // ハートビートの初期設定
    let initial_hb_config = HeartbeatConfig {
        interval_secs: cfg.agent.heartbeat_interval_secs,
        enabled: cfg.agent.heartbeat_enabled,
        heartbeat_channel_id: cfg.gateway.discord.heartbeat_channel_id,
    };

    let (heartbeat_config_tx, mut heartbeat_config_rx) =
        watch::channel(initial_hb_config.clone());

    let agent_ids: Vec<String> = {
        #[cfg(feature = "discord")]
        { cfg.gateway.discord.agent_ids.clone() }
        #[cfg(not(feature = "discord"))]
        { vec!["default".to_string()] }
    };

    // heartbeat設定変更を監視してループを再起動するタスク
    let heartbeat_agent_ids = agent_ids.clone();
    let heartbeat_db = state.db.clone();
    let heartbeat_llm_router = state.llm_router.clone();
    let heartbeat_default_model = state.default_model.clone();
    tokio::spawn(async move {
        let mut prev_config = initial_hb_config.clone();
        let mut current_shutdown_tx: Option<watch::Sender<bool>> = None;
        let mut _handles: Vec<tokio::task::JoinHandle<()>> = vec![];

        // ハートビートループを起動するヘルパークロージャ的ブロック
        // 初期起動
        if prev_config.enabled {
            tracing::info!(
                agent_ids = ?heartbeat_agent_ids,
                interval_secs = prev_config.interval_secs,
                "Starting heartbeat loops"
            );
            let (tx, rx_tmpl) = watch::channel(false);
            for agent_id in &heartbeat_agent_ids {
                let config_clone = prev_config.clone();
                let shutdown_rx = rx_tmpl.clone();
                let db = heartbeat_db.clone();
                let db_for_resolve = heartbeat_db.clone();
                let agent_id = agent_id.clone();
                let hb_discord_http = heartbeat_discord_http.clone();
                let hb_channel_id = heartbeat_channel_id_arc.clone();
                let llm_router_for_hb = heartbeat_llm_router.clone();
                let default_model_for_hb = heartbeat_default_model.clone();
                _handles.push(tokio::spawn(async move {
                    let callback: HeartbeatCallback = Box::new({
                        let db = db.clone();
                        let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                            resolve_agent_id(&conn, &agent_id)
                        } else {
                            agent_id.clone()
                        };
                        let agent_id_owned = resolved_agent_id;
                        let discord_http = hb_discord_http.clone();
                        let channel_id = hb_channel_id.clone();
                        let llm_for_hb = llm_router_for_hb.clone();
                        let model_for_hb = default_model_for_hb.clone();
                        move |_agent_id: &str, tick: u64| {
                            let _agent_id = _agent_id.to_string();
                            let db = db.clone();
                            let agent_id_owned = agent_id_owned.clone();
                            let discord_http = discord_http.clone();
                            let channel_id = channel_id.clone();
                            let llm_for_hb = llm_for_hb.clone();
                            let model_for_hb = model_for_hb.clone();
                            Box::pin(async move {
                            let prompt_opt = if let Ok(conn) = db.lock() {
                                Some(build_heartbeat_prompt(&conn, &agent_id_owned))
                            } else {
                                None
                            };
                            let decision = if let Some(prompt) = prompt_opt {
                                evaluate_heartbeat_action_llm(prompt, llm_for_hb, &model_for_hb).await
                            } else {
                                HeartbeatDecision::Idle
                            };
                            tracing::debug!(agent_id = %_agent_id, tick, decision = %decision, "Heartbeat tick");
                            if let Ok(conn) = db.lock() {
                                let decision_str = match &decision {
                                    HeartbeatDecision::Idle => "idle",
                                    HeartbeatDecision::Speak(_) => "speak",
                                    HeartbeatDecision::Learn => "learn",
                                    HeartbeatDecision::ManageSkills { .. } => "manage_skills",
                                };
                                if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
                                    &conn,
                                    &agent_id_owned,
                                    decision_str,
                                    None,
                                ) {
                                    tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat log: {}", e);
                                }
                            }
                            match &decision {
                                HeartbeatDecision::Speak(content) => {
                                    let content = content.clone();
                                    let discord_http = discord_http.clone();
                                    let channel_id = channel_id.clone();
                                    let agent_id_log = agent_id_owned.clone();
                                    tokio::spawn(async move {
                                        let http_opt = discord_http.lock().unwrap().clone();
                                        let ch_opt = *channel_id.lock().unwrap();
                                        if let (Some(_http), Some(ch_id)) = (http_opt.clone(), ch_opt) {
                                            #[cfg(feature = "discord")]
                                            {
                                                use serenity::model::id::ChannelId;
                                                use serenity::builder::CreateMessage;
                                                let ch = ChannelId::new(ch_id);
                                                if let Err(e) = ch.send_message(&_http, CreateMessage::new().content(&content)).await {
                                                    tracing::error!(agent_id = %agent_id_log, "Heartbeat send_speech failed: {e}");
                                                } else {
                                                    tracing::info!(agent_id = %agent_id_log, "Heartbeat spoke: {}", content);
                                                }
                                            }
                                            #[cfg(not(feature = "discord"))]
                                            {
                                                tracing::info!(agent_id = %agent_id_log, channel_id = ch_id, "Heartbeat Speak (discord disabled): {}", content);
                                            }
                                        } else {
                                            tracing::debug!(agent_id = %agent_id_log, "Heartbeat Speak: no Discord channel configured");
                                        }
                                    });
                                }
                                HeartbeatDecision::Learn => {
                                    let db = db.clone();
                                    let agent_id_log = agent_id_owned.clone();
                                    let tick_val = tick;
                                    tokio::spawn(async move {
                                        if let Ok(conn) = db.lock() {
                                            let memory = opencrab_db::queries::CuratedMemoryRow {
                                                id: uuid::Uuid::new_v4().to_string(),
                                                agent_id: agent_id_log.clone(),
                                                category: "reflection".to_string(),
                                                content: format!(
                                                    "ハートビート内省 (tick {}): 静かに自己を振り返る。",
                                                    tick_val
                                                ),
                                            };
                                            if let Err(e) = opencrab_db::queries::upsert_curated_memory(&conn, &memory) {
                                                tracing::error!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn failed: {e}");
                                            } else {
                                                tracing::info!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn: saved at tick {}", tick_val);
                                            }
                                        }
                                    });
                                }
                                HeartbeatDecision::Idle => {}
                                HeartbeatDecision::ManageSkills { .. } => {}
                            }
                            decision
                            }) // close Box::pin
                        }
                    });
                    heartbeat_loop(agent_id, config_clone, callback, shutdown_rx).await;
                }));
            }
            current_shutdown_tx = Some(tx);
        } else {
            tracing::info!("Heartbeat disabled (heartbeat_enabled = false in config)");
        }

        loop {
            if heartbeat_config_rx.changed().await.is_err() {
                break; // sender dropped
            }
            let new_config = heartbeat_config_rx.borrow().clone();
            if new_config.enabled != prev_config.enabled
                || new_config.interval_secs != prev_config.interval_secs
                || new_config.heartbeat_channel_id != prev_config.heartbeat_channel_id
            {
                tracing::info!(
                    enabled = new_config.enabled,
                    interval_secs = new_config.interval_secs,
                    "Heartbeat config changed, restarting loops"
                );
                // 既存ループを停止
                if let Some(tx) = current_shutdown_tx.take() {
                    let _ = tx.send(true);
                }
                _handles.clear();

                // heartbeat_channel_id_arcも更新
                if let Ok(mut guard) = heartbeat_channel_id_arc.lock() {
                    *guard = new_config.heartbeat_channel_id;
                }

                // 新設定で起動
                if new_config.enabled {
                    let (tx, rx_tmpl) = watch::channel(false);
                    for agent_id in &heartbeat_agent_ids {
                        let config_clone = new_config.clone();
                        let shutdown_rx = rx_tmpl.clone();
                        let db = heartbeat_db.clone();
                        let db_for_resolve = heartbeat_db.clone();
                        let agent_id = agent_id.clone();
                        let hb_discord_http = heartbeat_discord_http.clone();
                        let hb_channel_id = heartbeat_channel_id_arc.clone();
                        let llm_router_for_hb = heartbeat_llm_router.clone();
                        let default_model_for_hb = heartbeat_default_model.clone();
                        _handles.push(tokio::spawn(async move {
                            let callback: HeartbeatCallback = Box::new({
                                let db = db.clone();
                                let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                                    resolve_agent_id(&conn, &agent_id)
                                } else {
                                    agent_id.clone()
                                };
                                let agent_id_owned = resolved_agent_id;
                                let discord_http = hb_discord_http.clone();
                                let channel_id = hb_channel_id.clone();
                                let llm_for_hb = llm_router_for_hb.clone();
                                let model_for_hb = default_model_for_hb.clone();
                                move |_agent_id: &str, tick: u64| {
                                    let _agent_id = _agent_id.to_string();
                                    let db = db.clone();
                                    let agent_id_owned = agent_id_owned.clone();
                                    let discord_http = discord_http.clone();
                                    let channel_id = channel_id.clone();
                                    let llm_for_hb = llm_for_hb.clone();
                                    let model_for_hb = model_for_hb.clone();
                                    Box::pin(async move {
                                    let prompt_opt = if let Ok(conn) = db.lock() {
                                        Some(build_heartbeat_prompt(&conn, &agent_id_owned))
                                    } else {
                                        None
                                    };
                                    let decision = if let Some(prompt) = prompt_opt {
                                        evaluate_heartbeat_action_llm(prompt, llm_for_hb, &model_for_hb).await
                                    } else {
                                        HeartbeatDecision::Idle
                                    };
                                    tracing::debug!(agent_id = %_agent_id, tick, decision = %decision, "Heartbeat tick");
                                    if let Ok(conn) = db.lock() {
                                        let decision_str = match &decision {
                                            HeartbeatDecision::Idle => "idle",
                                            HeartbeatDecision::Speak(_) => "speak",
                                            HeartbeatDecision::Learn => "learn",
                                            HeartbeatDecision::ManageSkills { .. } => "manage_skills",
                                        };
                                        if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
                                            &conn,
                                            &agent_id_owned,
                                            decision_str,
                                            None,
                                        ) {
                                            tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat log: {}", e);
                                        }
                                    }
                                    match &decision {
                                        HeartbeatDecision::Speak(content) => {
                                            let content = content.clone();
                                            let discord_http = discord_http.clone();
                                            let channel_id = channel_id.clone();
                                            let agent_id_log = agent_id_owned.clone();
                                            tokio::spawn(async move {
                                                let http_opt = discord_http.lock().unwrap().clone();
                                                let ch_opt = *channel_id.lock().unwrap();
                                                if let (Some(_http), Some(ch_id)) = (http_opt.clone(), ch_opt) {
                                                    #[cfg(feature = "discord")]
                                                    {
                                                        use serenity::model::id::ChannelId;
                                                        use serenity::builder::CreateMessage;
                                                        let ch = ChannelId::new(ch_id);
                                                        if let Err(e) = ch.send_message(&_http, CreateMessage::new().content(&content)).await {
                                                            tracing::error!(agent_id = %agent_id_log, "Heartbeat send_speech failed: {e}");
                                                        } else {
                                                            tracing::info!(agent_id = %agent_id_log, "Heartbeat spoke: {}", content);
                                                        }
                                                    }
                                                    #[cfg(not(feature = "discord"))]
                                                    {
                                                        tracing::info!(agent_id = %agent_id_log, channel_id = ch_id, "Heartbeat Speak (discord disabled): {}", content);
                                                    }
                                                } else {
                                                    tracing::debug!(agent_id = %agent_id_log, "Heartbeat Speak: no Discord channel configured");
                                                }
                                            });
                                        }
                                        HeartbeatDecision::Learn => {
                                            let db = db.clone();
                                            let agent_id_log = agent_id_owned.clone();
                                            let tick_val = tick;
                                            tokio::spawn(async move {
                                                if let Ok(conn) = db.lock() {
                                                    let memory = opencrab_db::queries::CuratedMemoryRow {
                                                        id: uuid::Uuid::new_v4().to_string(),
                                                        agent_id: agent_id_log.clone(),
                                                        category: "reflection".to_string(),
                                                        content: format!(
                                                            "ハートビート内省 (tick {}): 静かに自己を振り返る。",
                                                            tick_val
                                                        ),
                                                    };
                                                    if let Err(e) = opencrab_db::queries::upsert_curated_memory(&conn, &memory) {
                                                        tracing::error!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn failed: {e}");
                                                    } else {
                                                        tracing::info!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn: saved at tick {}", tick_val);
                                                    }
                                                }
                                            });
                                        }
                                        HeartbeatDecision::Idle => {}
                                        HeartbeatDecision::ManageSkills { .. } => {}
                                    }
                                    decision
                                    }) // close Box::pin
                                }
                            });
                            heartbeat_loop(agent_id, config_clone, callback, shutdown_rx).await;
                        }));
                    }
                    current_shutdown_tx = Some(tx);
                }
                prev_config = new_config;
            }
        }
    });

    let _watcher_handle = opencrab_server::hot_reload::start_config_watcher(
        "config",
        state.tools_config.clone(),
        heartbeat_config_tx,
    );

    let app = create_router(state);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}
