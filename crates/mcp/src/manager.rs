//! per-agent の MCP 接続マネージャ。
//!
//! Nostr の `NostrGatewayManager` と同型。エージェント毎に有効な MCP サーバへ**永続接続**
//! を張り（起動時 `restore_from_db`、設定変更時 `reload_agent`）、各ターンでは
//! `provider_for` が既存接続を包んだ軽量な [`McpToolProvider`] を返す（ターン毎に
//! subprocess を起こさない）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::{info, warn};

use opencrab_db::Db;

use crate::actions::{ConnectedServer, McpToolProvider};
use crate::client::McpClient;
use crate::config::{config_from_row, McpServerConfig};

pub struct McpClientManager {
    db: Db,
    // agent_id → 接続済みサーバ群。ガードは await を跨がない。
    agents: RwLock<HashMap<String, Vec<ConnectedServer>>>,
}

impl McpClientManager {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            agents: RwLock::new(HashMap::new()),
        }
    }

    /// 全エージェントの enabled 設定を DB から復元して接続する（起動時）。
    pub async fn restore_from_db(&self) {
        // DbGuard は !Send なので、行の取得だけをロック内で済ませ、接続 await は外で行う。
        let rows = {
            let conn = match self.db.lock() {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "MCP: DB ロック失敗（restore 中止）");
                    return;
                }
            };
            opencrab_db::queries::list_all_enabled_agent_mcp_servers(&conn).unwrap_or_default()
        };
        let mut by_agent: HashMap<String, Vec<McpServerConfig>> = HashMap::new();
        for row in &rows {
            by_agent
                .entry(row.agent_id.clone())
                .or_default()
                .push(config_from_row(row));
        }
        for (agent_id, configs) in by_agent {
            self.start_agent(&agent_id, configs).await;
        }
    }

    /// 指定エージェントのサーバ群へ接続する（既存接続は置き換え）。接続失敗した
    /// サーバはスキップして他を活かす（fail-soft）。
    pub async fn start_agent(&self, agent_id: &str, configs: Vec<McpServerConfig>) {
        let mut connected = Vec::new();
        for cfg in configs {
            if !cfg.enabled {
                continue;
            }
            match McpClient::connect(&cfg).await {
                Ok(client) => {
                    info!(agent_id, server = %cfg.name, tools = client.tools().len(), "MCP サーバ接続");
                    connected.push(ConnectedServer {
                        server: Arc::new(client),
                        trusted_only: cfg.trusted_only,
                    });
                }
                Err(e) => {
                    warn!(agent_id, server = %cfg.name, error = %e, "MCP サーバ接続に失敗（このサーバはスキップ）");
                }
            }
        }
        // 旧接続は drop で kill される。
        self.agents
            .write()
            .unwrap()
            .insert(agent_id.to_string(), connected);
    }

    /// DB から該当エージェントの設定を読み直して再接続する（設定変更後に呼ぶ）。
    pub async fn reload_agent(&self, agent_id: &str) {
        let configs = self.agent_configs(agent_id);
        self.start_agent(agent_id, configs).await;
    }

    fn agent_configs(&self, agent_id: &str) -> Vec<McpServerConfig> {
        let conn = match self.db.lock() {
            Ok(c) => c,
            Err(e) => {
                warn!(agent_id, error = %e, "MCP: DB ロック失敗");
                return Vec::new();
            }
        };
        opencrab_db::queries::list_agent_mcp_servers(&conn, agent_id)
            .unwrap_or_default()
            .iter()
            .map(config_from_row)
            .filter(|c| c.enabled)
            .collect()
    }

    /// エージェントの接続を止める（drop で subprocess が kill される）。
    pub fn stop_agent(&self, agent_id: &str) {
        self.agents.write().unwrap().remove(agent_id);
    }

    /// 本ターン用の MCP ツール源を返す。`caller_is_trusted` で trusted_only サーバの
    /// 出し分けを行う。接続が無ければ空プロバイダ（ツール0件）。
    pub fn provider_for(&self, agent_id: &str, caller_is_trusted: bool) -> McpToolProvider {
        let servers = self
            .agents
            .read()
            .unwrap()
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        McpToolProvider::new(servers, caller_is_trusted)
    }

    /// 接続済みサーバの (name, tools 数) 一覧（ダッシュボード表示用）。
    pub fn connected_status(&self, agent_id: &str) -> Vec<(String, usize)> {
        self.agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|v| {
                v.iter()
                    .map(|s| (s.server.server_name().to_string(), s.server.tools().len()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 何らかのサーバに接続済みか。
    pub fn has_connections(&self, agent_id: &str) -> bool {
        self.agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    pub fn shutdown_all(&self) {
        self.agents.write().unwrap().clear();
    }
}
