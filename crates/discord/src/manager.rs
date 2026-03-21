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

use crate::AgentRunner;

struct AgentGatewayEntry {
    gateway: Arc<DiscordGateway>,
    handle: JoinHandle<()>,
}

pub struct DiscordGatewayManager<T: AgentRunner> {
    gateways: RwLock<HashMap<String, AgentGatewayEntry>>,
    state: T,
}

impl<T: AgentRunner> DiscordGatewayManager<T> {
    pub fn new(state: T) -> Self {
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

        let workspace_path = self.state.workspace_base()
            .replace("{agent_id}", agent_id);
        let workspace_root = std::path::PathBuf::from(workspace_path);

        let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
            crate::DiscordGatewayActions::new(
                gateway.http().clone(),
                self.state.db().clone(),
                agent_id.to_string(),
                self.state.tools_config().clone(),
                Some(self.state.create_llm_client()),
                self.state.default_model(),
                workspace_root,
            ),
        );

        let loop_state = self.state.clone();
        let loop_gateway = gateway.clone();
        let agent_ids = vec![agent_id.to_string()];
        let owner = owner_discord_id.to_string();

        let handle = tokio::spawn(async move {
            crate::run_discord_loop(
                loop_gateway,
                loop_state,
                agent_ids,
                gateway_actions,
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

    /// Get the HTTP client for a per-agent gateway.
    pub async fn get_http_for_agent(&self, agent_id: &str) -> Option<Arc<serenity::http::Http>> {
        let gateways = self.gateways.read().await;
        gateways.get(agent_id).map(|e| e.gateway.http().clone())
    }

    /// Restore all enabled agent Discord configs from DB and start their gateways.
    pub async fn restore_from_db(&self) {
        let configs = {
            let conn = self.state.db().lock().unwrap();
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
