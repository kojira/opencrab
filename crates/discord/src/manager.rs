//! Per-agent Discord Bot gateway manager.
//!
//! Each agent can have its own Discord Bot token, managed independently.
//! `DiscordGatewayManager` handles lifecycle (start/stop) for all per-agent gateways.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::task::JoinHandle;
use tracing::{error, info};

use opencrab_gateway::DiscordGateway;

use crate::AgentRunner;
use crate::PendingInteractionRegistry;
use opencrab_actions::subtask::SubtaskRegistry;

struct AgentGatewayEntry {
    gateway: Arc<DiscordGateway>,
    handle: JoinHandle<()>,
}

/// per-agent ゲートウェイを起動する**前**の owner 前処理: 正規化して、未設定なら警告する。
///
/// owner は入口で正規化する。DB に前後空白付きで保存された既存行でも、
/// 「DM は通るのに owner 専用 UI だけ無言で拒否される」半端な状態を作らない
/// （下位の form/modal 側は生比較のまま。判定述語の共通化は #174）。
///
/// per-agent 経路は共有（TOML）ゲートウェイ側の起動警告に載らないので、ここでも
/// owner 未設定を知らせる（復元経路 `restore_from_db` も通る）。
///
/// `start_agent_gateway` 本体は `DiscordGateway::start()` で実ネットワークに出るため
/// そのままではテストできない。ネットワークに触らない前処理だけをこの関数に切り出し、
/// 戻り値（正規化済み owner）を呼び出し側に使わせることで、警告と正規化の両方を
/// 単体テストで押さえる。
///
/// `#[deny(dead_code)]` は「この関数が呼ばれ続けること」を保証するための保険。
/// 将来 `start_agent_gateway` をリファクタして呼び出しを落とすと、警告ではなく
/// コンパイルエラーになる（CI は警告では落ちないため、警告では歯止めにならない）。
/// 呼び出しが消えると owner の入口正規化も消え、レガシー空白付きの行で
/// 「DM は通るのに owner 専用 UI だけ無言で拒否」が復活してしまう。
#[deny(dead_code)]
fn prepare_owner_for_gateway(agent_id: &str, owner_discord_id: &str) -> String {
    let owner_discord_id = owner_discord_id.trim();
    crate::owner_warning::warn_if_agent_gateway_owner_unset(agent_id, owner_discord_id);
    owner_discord_id.to_string()
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
        // 起動前の owner 前処理（正規化 + 未設定警告）。テストは下の `tests` モジュール。
        let owner_normalized = prepare_owner_for_gateway(agent_id, owner_discord_id);
        let owner_discord_id = owner_normalized.as_str();

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
        // ループ（auto-dispatch）と gateway_actions（cancel_subtask）で同一 registry を
        // 共有し、auto-dispatch した subtask を停止可能にする（RFC #152 S3a / P0）。
        let subtask_registry_for_loop = subtask_registry.clone();

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
                subtask_registry_for_loop,
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

#[cfg(test)]
mod tests {
    use super::prepare_owner_for_gateway;
    use crate::owner_warning::capture::captured_logs;

    /// 起動経路が owner を正規化して渡す（DB のレガシー行が空白付きでも同じ）。
    #[test]
    fn start_path_normalizes_owner() {
        assert_eq!(
            prepare_owner_for_gateway("crab", "  123456789012345678\n"),
            "123456789012345678"
        );
        assert_eq!(prepare_owner_for_gateway("crab", "   "), "");
    }

    /// 起動経路そのものが owner 未設定の警告を出す。
    ///
    /// `owner_warning` の純関数テストだけでは「呼ばれているか」を保証できない。
    /// ここでは起動前処理を実際に呼び、warn イベントが出ることを確認する。
    #[test]
    fn start_path_warns_when_owner_is_unset() {
        for owner in ["", " ", " \t\n"] {
            let logs = captured_logs(|| {
                prepare_owner_for_gateway("agent-under-test", owner);
            });
            assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
            assert!(
                logs.contains("empty owner_discord_id"),
                "owner={owner:?} で本文が出ること: {logs}"
            );
            assert!(
                logs.contains("agent-under-test"),
                "どのエージェントか分かること: {logs}"
            );
        }
    }

    /// owner 設定済みなら起動経路は黙る（「常に出ている警告」を作らない）。
    #[test]
    fn start_path_is_silent_when_owner_is_set() {
        let logs = captured_logs(|| {
            prepare_owner_for_gateway("crab", "  123456789012345678  ");
        });
        assert!(logs.trim().is_empty(), "余計な警告を出さないこと: {logs}");
    }
}
