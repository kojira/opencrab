use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_server::{config, create_router, AppState};

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
        workspace_base: "data".to_string(),
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

    let app = create_router(state);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}
