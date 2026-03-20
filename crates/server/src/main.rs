use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_server::{config, create_router, AppState};
use opencrab_core::heartbeat::{HeartbeatCallback, HeartbeatConfig, HeartbeatDecision, heartbeat_loop};
use tokio::sync::watch;

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
                _handles.push(tokio::spawn(async move {
                    let callback: HeartbeatCallback = Box::new({
                        let db = db.clone();
                        let agent_id_owned = agent_id.clone();
                        move |_agent_id: &str, tick: u64| {
                            let decision = HeartbeatDecision::Idle;
                            tracing::debug!(agent_id = %_agent_id, tick, "Heartbeat tick (idle)");
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
                        _handles.push(tokio::spawn(async move {
                            let callback: HeartbeatCallback = Box::new({
                                let db = db.clone();
                                let agent_id_owned = agent_id.clone();
                                move |_agent_id: &str, tick: u64| {
                                    let decision = HeartbeatDecision::Idle;
                                    tracing::debug!(agent_id = %_agent_id, tick, "Heartbeat tick (idle)");
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
