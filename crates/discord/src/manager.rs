//! Per-agent Discord Bot gateway manager.
//!
//! Each agent can have its own Discord Bot token, managed independently.
//! `DiscordGatewayManager` handles lifecycle (start/stop) for all per-agent gateways.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::task::JoinHandle;
use tracing::{error, info};

use opencrab_gateway::DiscordGateway;

use crate::gateway_actions::SubtaskRegistry;
use crate::AgentRunner;
use crate::PendingInteractionRegistry;

struct AgentGatewayEntry {
    gateway: Arc<DiscordGateway>,
    handle: JoinHandle<()>,
}

pub struct DiscordGatewayManager<T: AgentRunner> {
    // std RwLock（tokio ではない）: is_running を同期メソッドにするため。
    // ガードを await 跨ぎで保持しないこと（各メソッドでスコープを閉じる）。
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

        let pending_interaction_registry: PendingInteractionRegistry =
            Arc::new(dashmap::DashMap::new());
        let form_modal_resolver = Some(crate::form_modal::form_modal_resolver(
            pending_interaction_registry.clone(),
        ));
        let gateway = Arc::new(DiscordGateway::with_form_modal_resolver(
            token,
            form_modal_resolver,
        ));
        gateway.start().await?;

        let subtask_registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());

        // Create event channel for A2UI and other async events
        let (event_tx, event_rx) = crate::message_loop::create_event_channel();

        // Cleanup stale pending interactions from previous runs
        self.state.cleanup_stale_interactions();

        let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
            crate::DiscordGatewayActions::new(
                gateway.http().clone(),
                self.state.db().clone(),
                self.state.tools_config().clone(),
                Some(self.state.create_llm_client()),
                self.state.default_model(),
                self.state.workspace_base().to_string(),
                subtask_registry,
                None,
            )
            .with_a2ui(pending_interaction_registry.clone(), event_tx.clone())
            .with_owner_discord_id(owner_discord_id),
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
                Some(pending_interaction_registry),
                Some((event_tx, event_rx)),
                // per-agent ゲートウェイは enabled な設定から起動される側なので
                // 専用設定スキップは無効（true にすると自分自身を skip してしまう）。
                false,
                // VC 対話 v1 は共有（TOML）ゲートウェイのみ対応。per-agent 側は未配線。
                None,
            )
            .await;
        });

        {
            let mut gateways = self.gateways.write().unwrap();
            gateways.insert(agent_id.to_string(), AgentGatewayEntry { gateway, handle });
        }

        info!(agent_id = %agent_id, "Per-agent Discord gateway started");
        Ok(())
    }

    /// Stop a per-agent Discord gateway.
    pub async fn stop_agent_gateway(&self, agent_id: &str) {
        let entry = {
            let mut gateways = self.gateways.write().unwrap();
            gateways.remove(agent_id)
        };

        if let Some(entry) = entry {
            entry.gateway.shutdown().await;
            entry.handle.abort();
            info!(agent_id = %agent_id, "Per-agent Discord gateway stopped");
        }
    }

    /// Check if a per-agent gateway is running.
    ///
    /// 同期メソッド: 共有ゲートウェイのメッセージループが per-message で
    /// 「専用ゲートウェイが実際に稼働しているか」を判定するのに使う（#40）。
    pub fn is_running(&self, agent_id: &str) -> bool {
        let gateways = self.gateways.read().unwrap();
        gateways
            .get(agent_id)
            .map(|e| !e.handle.is_finished())
            .unwrap_or(false)
    }

    /// Get the HTTP client for a per-agent gateway.
    pub fn get_http_for_agent(&self, agent_id: &str) -> Option<Arc<serenity::http::Http>> {
        let gateways = self.gateways.read().unwrap();
        gateways.get(agent_id).map(|e| e.gateway.http().clone())
    }

    /// Restore all enabled agent Discord configs from DB and start their gateways.
    pub async fn restore_from_db(&self) {
        for cfg in self.state.list_enabled_discord_configs() {
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

    /// Shutdown all per-agent gateways.
    pub async fn shutdown_all(&self) {
        let entries: Vec<(String, AgentGatewayEntry)> = {
            let mut gateways = self.gateways.write().unwrap();
            gateways.drain().collect()
        };

        for (agent_id, entry) in entries {
            entry.gateway.shutdown().await;
            entry.handle.abort();
            info!(agent_id = %agent_id, "Per-agent Discord gateway stopped (shutdown_all)");
        }
    }
}
