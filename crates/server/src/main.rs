use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_server::{config, create_router, AppState};
use opencrab_core::heartbeat::{HeartbeatCallback, HeartbeatConfig, HeartbeatDecision, heartbeat_loop};
use tokio::sync::watch;

#[cfg(feature = "discord")]
type DiscordHttpArc = Arc<Mutex<Option<Arc<serenity::http::Http>>>>;
#[cfg(not(feature = "discord"))]
type DiscordHttpArc = Arc<Mutex<Option<()>>>;

/// ハートビート用セッションを取得または作成する。
fn get_or_create_heartbeat_session(
    db: &Arc<Mutex<rusqlite::Connection>>,
    agent_id: &str,
    channel_id: &str,
) -> String {
    let session_id = format!("heartbeat-{}-{}", agent_id, channel_id);
    let conn = db.lock().unwrap();
    if let Ok(Some(_)) = opencrab_db::queries::get_session(&conn, &session_id) {
        return session_id;
    }
    let session = opencrab_db::queries::SessionRow {
        id: session_id.clone(),
        mode: "heartbeat".to_string(),
        theme: "ハートビート自律行動".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::json!([agent_id]).to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
        tracing::warn!("Failed to create heartbeat session: {e}");
    }
    session_id
}

/// ハートビートコールバックを生成する。
/// 初期起動とhot-reload再起動の両方で使用。
fn make_heartbeat_callback(
    db: Arc<Mutex<rusqlite::Connection>>,
    agent_id_owned: String,
    discord_http: DiscordHttpArc,
    state: AppState,
    global_interval_secs: u64,
    last_channel_ticks: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
) -> HeartbeatCallback {
    Box::new(move |_agent_id: &str, tick: u64| {
        let _agent_id = _agent_id.to_string();
        let db = db.clone();
        let agent_id_owned = agent_id_owned.clone();
        let discord_http = discord_http.clone();
        let state = state.clone();
        let global_interval_secs = global_interval_secs;
        let last_channel_ticks = last_channel_ticks.clone();
        Box::pin(async move {
            // heartbeat_enabled=trueのチャンネルを全取得
            let whitelisted_channels: Vec<(String, String, Option<u64>)> = {
                let conn = db.lock().unwrap();
                match opencrab_db::queries::list_heartbeat_channels(&conn) {
                    Ok(channels) => channels.into_iter()
                        .map(|c| (c.channel_id.clone(), c.channel_name.clone(), c.heartbeat_interval_secs))
                        .collect(),
                    Err(e) => {
                        tracing::warn!("Failed to list whitelisted channels: {e}");
                        vec![]
                    }
                }
            };

            if whitelisted_channels.is_empty() {
                tracing::debug!(agent_id = %agent_id_owned, tick, "No whitelisted channels, skipping heartbeat tick");
                return HeartbeatDecision::Idle;
            }

            // 最後の決定を返す（全チャンネルを処理した後）
            let mut last_decision = HeartbeatDecision::Idle;

            for (channel_id_str, channel_name, channel_interval_secs) in &whitelisted_channels {
                // per-channel interval チェック
                let effective_interval = channel_interval_secs
                    .unwrap_or(global_interval_secs);
                let should_fire = {
                    let ticks = last_channel_ticks.lock().unwrap();
                    let now = std::time::Instant::now();
                    let last = ticks.get(channel_id_str.as_str());
                    match last {
                        None => true,
                        Some(last_time) => now.duration_since(*last_time).as_secs() >= effective_interval,
                    }
                };
                if !should_fire {
                    tracing::debug!(
                        agent_id = %agent_id_owned,
                        channel_id = %channel_id_str,
                        effective_interval,
                        "Heartbeat: channel interval not elapsed, skipping"
                    );
                    continue;
                }
                // last_tickを更新
                {
                    let mut ticks = last_channel_ticks.lock().unwrap();
                    ticks.insert(channel_id_str.clone(), std::time::Instant::now());
                }

                tracing::debug!(
                    agent_id = %agent_id_owned,
                    channel_id = %channel_id_str,
                    channel_name = %channel_name,
                    tick,
                    "Heartbeat tick for channel"
                );

                // 1. チャンネルごとのセッション取得/作成
                let session_id = get_or_create_heartbeat_session(&db, &agent_id_owned, channel_id_str);

                // 2. ハートビートプロンプトをsession_logsに挿入
                {
                    let conn = db.lock().unwrap();
                    let prompt = format!(
                        "[ハートビート] チャンネル「{}」で今この瞬間、自律的に何をするか判断してください。SPEAK/LEARN/IDLEから選んでください。SPEAKの場合は'SPEAK: <メッセージ>'の形式で一言。発言は30分に1回以下が望ましい。",
                        channel_name
                    );
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id_owned.clone(),
                        session_id: session_id.clone(),
                        log_type: "system".to_string(),
                        content: prompt,
                        speaker_id: Some("heartbeat".to_string()),
                        turn_number: None,
                        metadata_json: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                        tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat prompt log: {e}");
                        continue;
                    }
                }

                // 3-4. エージェントコンテキストと会話文字列を構築
                let (system_prompt, agent_name, conversation) = {
                    let conn = db.lock().unwrap();
                    let (sp, name) = opencrab_server::process::build_agent_context(&conn, &agent_id_owned, "ハートビート自律行動");
                    let conv = opencrab_server::process::build_conversation_string(&conn, &session_id);
                    (sp, name, conv)
                };

                // 5. run_agent_response を呼び出す
                let engine_result = opencrab_server::process::run_agent_response(
                    &state,
                    &agent_id_owned,
                    &agent_name,
                    &session_id,
                    &system_prompt,
                    &conversation,
                    "heartbeat",
                    None,
                    opencrab_actions::CallerIdentity::Owner,
                    &[],
                    0,
                    None,   // trigger_message_id
                    None,
                ).await;

                let decision = match engine_result {
                    Ok(result) => {
                        // 6. エージェント応答をsession_logsに記録
                        {
                            let conn = db.lock().unwrap();
                            let log = opencrab_db::queries::SessionLogRow {
                                id: None,
                                agent_id: agent_id_owned.clone(),
                                session_id: session_id.clone(),
                                log_type: "speech".to_string(),
                                content: result.response.clone(),
                                speaker_id: Some(agent_id_owned.clone()),
                                turn_number: None,
                                metadata_json: None,
                            };
                            if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                                tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat response log: {e}");
                            }
                        }

                        // 7. 応答からSPEAK/LEARN/IDLEを解析
                        let response_text = result.response.trim().to_string();
                        if response_text.contains("SPEAK:") {
                            let content = response_text.lines()
                                .find(|l| l.contains("SPEAK:"))
                                .and_then(|l| l.splitn(2, "SPEAK:").nth(1))
                                .unwrap_or("").trim().to_string();
                            if !content.is_empty() {
                                HeartbeatDecision::Speak(content)
                            } else {
                                HeartbeatDecision::Idle
                            }
                        } else if response_text.to_uppercase().contains("LEARN") {
                            HeartbeatDecision::Learn
                        } else {
                            HeartbeatDecision::Idle
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Heartbeat agent response failed for channel {}: {e}", channel_id_str);
                        HeartbeatDecision::Idle
                    }
                };

                tracing::debug!(agent_id = %_agent_id, tick, channel_id = %channel_id_str, decision = %decision, "Heartbeat tick result");

                // heartbeat_logに記録
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
                        Some(&format!("channel_id={}", channel_id_str)),
                    ) {
                        tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat log: {}", e);
                    }
                }

                // Speak/Learn後続処理
                match &decision {
                    HeartbeatDecision::Speak(content) => {
                        let content = content.clone();
                        let discord_http = discord_http.clone();
                        let channel_id_u64: Option<u64> = channel_id_str.parse().ok();
                        let agent_id_log = agent_id_owned.clone();
                        let ch_id_str = channel_id_str.clone();
                        tokio::spawn(async move {
                            let http_opt = discord_http.lock().unwrap().clone();
                            if let (Some(_http), Some(ch_id)) = (http_opt.clone(), channel_id_u64) {
                                #[cfg(feature = "discord")]
                                {
                                    use serenity::model::id::ChannelId;
                                    use serenity::builder::CreateMessage;
                                    let ch = ChannelId::new(ch_id);
                                    if let Err(e) = ch.send_message(&_http, CreateMessage::new().content(&content)).await {
                                        tracing::error!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat send_speech failed: {e}");
                                    } else {
                                        tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat spoke: {}", content);
                                    }
                                }
                                #[cfg(not(feature = "discord"))]
                                {
                                    tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat Speak (discord disabled): {}", content);
                                }
                            } else {
                                tracing::debug!(agent_id = %agent_id_log, "Heartbeat Speak: no Discord http or invalid channel_id");
                            }
                        });
                    }
                    HeartbeatDecision::Learn => {
                        let db = db.clone();
                        let agent_id_log = agent_id_owned.clone();
                        let tick_val = tick;
                        let ch_id_str = channel_id_str.clone();
                        tokio::spawn(async move {
                            if let Ok(conn) = db.lock() {
                                let memory = opencrab_db::queries::CuratedMemoryRow {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    agent_id: agent_id_log.clone(),
                                    category: "reflection".to_string(),
                                    content: format!(
                                        "ハートビート内省 (tick {}, channel {}): 静かに自己を振り返る。",
                                        tick_val, ch_id_str
                                    ),
                                };
                                if let Err(e) = opencrab_db::queries::upsert_curated_memory(&conn, &memory) {
                                    tracing::error!(agent_id = %agent_id_log, "Heartbeat reflect_and_learn failed: {e}");
                                } else {
                                    tracing::info!(agent_id = %agent_id_log, channel_id = %ch_id_str, "Heartbeat reflect_and_learn: saved at tick {}", tick_val);
                                }
                            }
                        });
                    }
                    HeartbeatDecision::Idle => {}
                    HeartbeatDecision::ManageSkills { .. } => {}
                }

                last_decision = decision;
            }

            last_decision
        })
    })
}

/// config名またはUUIDのagent_idを、DBのUUIDに解決する。
/// "crab"のような名前が渡された場合、find_agentsで検索してUUIDを返す。
fn resolve_agent_id(conn: &rusqlite::Connection, agent_id: &str) -> String {
    // まず直接lookupを試みる
    if let Ok(Some(_)) = opencrab_db::queries::get_identity(conn, agent_id) {
        return agent_id.to_string();
    }
    // 名前で検索（完全一致またはUUID前方一致のみ。部分一致は複数エージェント時に誤マッチするため使わない）
    if let Ok(agents) = opencrab_db::queries::find_agents(conn, agent_id) {
        if let Some((uuid, _name)) = agents.iter().find(|(id, name)| {
            id.starts_with(agent_id) || name.to_lowercase() == agent_id.to_lowercase()
        }) {
            tracing::info!(config_id = %agent_id, uuid = %uuid, "Resolved agent_id config name to UUID (exact match)");
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

    // DB内の許可コマンドをtools_configにマージ
    let mut tools_cfg = cfg.tools.clone();
    if let Ok(agents) = opencrab_db::queries::find_agents(&conn, "") {
        for (agent_id, _name) in &agents {
            if let Ok(db_commands) = opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id) {
                if let Some(ref mut shell) = tools_cfg.shell {
                    for cmd in db_commands {
                        if !shell.allowed_commands.contains(&cmd) {
                            shell.allowed_commands.push(cmd);
                        }
                    }
                }
            }
        }
    }

    #[allow(unused_mut)]
    let mut state = AppState {
        db: Arc::new(Mutex::new(conn)),
        llm_router: Arc::new(llm_router),
        workspace_base: cfg.agent.workspace_path.clone(),
        tools_config: Arc::new(std::sync::RwLock::new(tools_cfg)),
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
                    .map(|agent_id| resolve_agent_id(&conn, agent_id))
                    .filter(|agent_id| {
                        match opencrab_db::queries::get_identity(&conn, agent_id) {
                            Ok(Some(_)) => true,
                            _ => {
                                tracing::warn!("Agent '{}' not found in database, skipping", agent_id);
                                false
                            }
                        }
                    })
                    .collect()
            };

            if valid_agent_ids.is_empty() {
                tracing::error!("No valid agents found for Discord gateway, not starting");
            } else {
                let gateway = Arc::new(opencrab_gateway::DiscordGateway::new(&discord_cfg.token));
                gateway.start().await?;

                let first_agent_id = valid_agent_ids.first().cloned().unwrap_or_default();
                let workspace_path = state.workspace_base.replace("{agent_id}", &first_agent_id);
                let workspace_root = std::path::PathBuf::from(workspace_path);
                let subtask_registry: opencrab_discord::SubtaskRegistry = Arc::new(dashmap::DashMap::new());
                let completion_registry: opencrab_discord::CompletionRegistry = Arc::new(dashmap::DashMap::new());
                let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
                    opencrab_discord::DiscordGatewayActions::new(
                        gateway.http().clone(),
                        state.db.clone(),
                        first_agent_id,
                        state.tools_config.clone(),
                        Some(Arc::new(opencrab_server::llm_adapter::LlmRouterAdapter::new(state.llm_router.clone()))),
                        state.default_model.clone(),
                        workspace_root,
                        subtask_registry,
                        completion_registry.clone(),
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
                        completion_registry,
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

        // Per-agentゲートウェイのHTTPクライアントをheartbeatに設定
        let heartbeat_agent_id_for_http = {
            let conn = state.db.lock().unwrap();
            cfg.gateway.discord.agent_ids.first()
                .map(|id| resolve_agent_id(&conn, id))
                .unwrap_or_default()
        };
        if let Some(http) = manager.get_http_for_agent(&heartbeat_agent_id_for_http).await {
            *heartbeat_discord_http.lock().unwrap() = Some(http);
            tracing::info!(agent_id = %heartbeat_agent_id_for_http, "Set heartbeat Discord HTTP from per-agent gateway");
        }
        if let Some(ch_id) = cfg.gateway.discord.heartbeat_channel_id {
            *heartbeat_channel_id_arc.lock().unwrap() = Some(ch_id);
            tracing::info!(channel_id = %ch_id, "Set heartbeat channel ID from config");
        }

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
    let heartbeat_state = state.clone();
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
                let _hb_channel_id = heartbeat_channel_id_arc.clone();
                let state_for_hb = heartbeat_state.clone();
                let last_channel_ticks = Arc::new(Mutex::new(std::collections::HashMap::<String, std::time::Instant>::new()));
                _handles.push(tokio::spawn(async move {
                    let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                        resolve_agent_id(&conn, &agent_id)
                    } else {
                        agent_id.clone()
                    };
                    let callback = make_heartbeat_callback(
                        db,
                        resolved_agent_id,
                        hb_discord_http,
                        state_for_hb,
                        config_clone.interval_secs,
                        last_channel_ticks,
                    );
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
                        let _hb_channel_id = heartbeat_channel_id_arc.clone();
                        let state_for_hb = heartbeat_state.clone();
                        let last_channel_ticks = Arc::new(Mutex::new(std::collections::HashMap::<String, std::time::Instant>::new()));
                        _handles.push(tokio::spawn(async move {
                            let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                                resolve_agent_id(&conn, &agent_id)
                            } else {
                                agent_id.clone()
                            };
                            let callback = make_heartbeat_callback(
                                db,
                                resolved_agent_id,
                                hb_discord_http,
                                state_for_hb,
                                config_clone.interval_secs,
                                last_channel_ticks,
                            );
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
