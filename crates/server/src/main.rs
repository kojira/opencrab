use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_server::{config, create_router, AppState};
use opencrab_core::heartbeat::{HeartbeatCallback, HeartbeatConfig, HeartbeatDecision, heartbeat_loop};
use tokio::sync::watch;

fn evaluate_heartbeat_action(
    conn: &rusqlite::Connection,
    agent_id: &str,
    tick: u64,
) -> HeartbeatDecision {
    if tick % 10 == 0 {
        return HeartbeatDecision::Learn;
    }
    if tick % 3 == 0 {
        let persona = opencrab_db::queries::get_soul(conn, agent_id)
            .ok()
            .flatten()
            .map(|s| s.persona_name)
            .unwrap_or_else(|| "Agent".to_string());
        let messages = [
            format!("⚡ ハートビート: {}として思考中です。", persona),
            format!("静かに存在しています。"),
            format!("⚡ 自律的に動作中。記憶を整理しています。"),
        ];
        let content = messages[(tick as usize / 3) % messages.len()].clone();
        return HeartbeatDecision::Speak(content);
    }
    HeartbeatDecision::Idle
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

                let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
                    opencrab_discord::DiscordGatewayActions::new(
                        gateway.http().clone(),
                        state.db.clone(),
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
                let agent_id = agent_id.clone();
                let hb_discord_http = heartbeat_discord_http.clone();
                let hb_channel_id = heartbeat_channel_id_arc.clone();
                _handles.push(tokio::spawn(async move {
                    let callback: HeartbeatCallback = Box::new({
                        let db = db.clone();
                        let agent_id_owned = agent_id.clone();
                        let discord_http = hb_discord_http.clone();
                        let channel_id = hb_channel_id.clone();
                        move |_agent_id: &str, tick: u64| {
                            let decision = if let Ok(conn) = db.lock() {
                                evaluate_heartbeat_action(&conn, &agent_id_owned, tick)
                            } else {
                                HeartbeatDecision::Idle
                            };
                            tracing::debug!(agent_id = %_agent_id, tick, decision = %decision, "Heartbeat tick");
                            if let Ok(conn) = db.lock() {
                                let decision_str = match &decision {
                                    HeartbeatDecision::Idle => "idle",
                                    HeartbeatDecision::Speak(_) => "speak",
                                    HeartbeatDecision::Learn => "learn",
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
                            }
                            decision
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

                // 新設定で起動
                if new_config.enabled {
                    let (tx, rx_tmpl) = watch::channel(false);
                    for agent_id in &heartbeat_agent_ids {
                        let config_clone = new_config.clone();
                        let shutdown_rx = rx_tmpl.clone();
                        let db = heartbeat_db.clone();
                        let agent_id = agent_id.clone();
                        let hb_discord_http = heartbeat_discord_http.clone();
                        let hb_channel_id = heartbeat_channel_id_arc.clone();
                        _handles.push(tokio::spawn(async move {
                            let callback: HeartbeatCallback = Box::new({
                                let db = db.clone();
                                let agent_id_owned = agent_id.clone();
                                let discord_http = hb_discord_http.clone();
                                let channel_id = hb_channel_id.clone();
                                move |_agent_id: &str, tick: u64| {
                                    let decision = if let Ok(conn) = db.lock() {
                                        evaluate_heartbeat_action(&conn, &agent_id_owned, tick)
                                    } else {
                                        HeartbeatDecision::Idle
                                    };
                                    tracing::debug!(agent_id = %_agent_id, tick, decision = %decision, "Heartbeat tick");
                                    if let Ok(conn) = db.lock() {
                                        let decision_str = match &decision {
                                            HeartbeatDecision::Idle => "idle",
                                            HeartbeatDecision::Speak(_) => "speak",
                                            HeartbeatDecision::Learn => "learn",
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
                                    }
                                    decision
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
