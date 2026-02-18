//! Per-agent Discord Bot gateway manager.
//!
//! Each agent can have its own Discord Bot token, managed independently.
//! `DiscordGatewayManager` handles lifecycle (start/stop) for all per-agent gateways.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use opencrab_gateway::DiscordGateway;

use crate::AppState;

struct AgentGatewayEntry {
    gateway: Arc<DiscordGateway>,
    handle: JoinHandle<()>,
}

pub struct DiscordGatewayManager {
    gateways: RwLock<HashMap<String, AgentGatewayEntry>>,
    state: AppState,
}

impl DiscordGatewayManager {
    pub fn new(state: AppState) -> Self {
        Self {
            gateways: RwLock::new(HashMap::new()),
            state,
        }
    }

    /// Start a per-agent Discord gateway with the given token.
    pub async fn start_agent_gateway(
        &self,
        agent_id: &str,
        token: &str,
        owner_discord_id: &str,
    ) -> anyhow::Result<()> {
        // Stop existing gateway for this agent if running.
        self.stop_agent_gateway(agent_id).await;

        let gateway = Arc::new(DiscordGateway::new(token));
        gateway.start().await?;

        let gateway_admin: Arc<dyn opencrab_actions::GatewayAdmin> = Arc::new(
            crate::discord_admin_impl::SerenityGatewayAdmin::new(gateway.http().clone()),
        );

        let loop_state = self.state.clone();
        let loop_gateway = gateway.clone();
        let agent_ids = vec![agent_id.to_string()];
        let owner = owner_discord_id.to_string();

        let handle = tokio::spawn(async move {
            crate::discord::run_discord_loop(
                loop_gateway,
                loop_state,
                agent_ids,
                gateway_admin,
                owner,
            )
            .await;
        });

        let mut gateways = self.gateways.write().await;
        gateways.insert(
            agent_id.to_string(),
            AgentGatewayEntry { gateway, handle },
        );

        info!(agent_id = %agent_id, "Per-agent Discord gateway started");
        Ok(())
    }

    /// Stop a per-agent Discord gateway.
    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        let entry = {
            let mut gateways = self.gateways.write().await;
            gateways.remove(agent_id)
        };

        if let Some(entry) = entry {
            entry.gateway.shutdown().await;
            entry.handle.abort();
            info!(agent_id = %agent_id, "Per-agent Discord gateway stopped");
        }
    }

    /// Check if a per-agent gateway is running.
    pub async fn is_running(&self, agent_id: &str) -> bool {
        let gateways = self.gateways.read().await;
        gateways
            .get(agent_id)
            .map(|e| !e.handle.is_finished())
            .unwrap_or(false)
    }

    /// Restore all enabled agent Discord configs from DB and start their gateways.
    pub async fn restore_from_db(&self) {
        let configs = {
            let conn = self.state.db.lock().unwrap();
            opencrab_db::queries::list_enabled_agent_discord_configs(&conn)
        };

        match configs {
            Ok(configs) => {
                for cfg in configs {
                    if let Err(e) = self
                        .start_agent_gateway(&cfg.agent_id, &cfg.bot_token, &cfg.owner_discord_id)
                        .await
                    {
                        error!(
                            agent_id = %cfg.agent_id,
                            error = %e,
                            "Failed to restore per-agent Discord gateway"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to load agent discord configs from DB");
            }
        }
    }

    /// Shutdown all per-agent gateways.
    pub async fn shutdown_all(&self) {
        let entries: Vec<(String, AgentGatewayEntry)> = {
            let mut gateways = self.gateways.write().await;
            gateways.drain().collect()
        };

        for (agent_id, entry) in entries {
            entry.gateway.shutdown().await;
            entry.handle.abort();
            info!(agent_id = %agent_id, "Per-agent Discord gateway stopped (shutdown_all)");
        }
    }
}
