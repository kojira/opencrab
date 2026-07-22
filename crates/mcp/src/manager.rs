//! per-agent の MCP 接続マネージャ。
//!
//! Nostr の `NostrGatewayManager` と同型。エージェント毎に有効な MCP サーバへ**永続接続**
//! を張り（起動時 `restore_from_db`、設定変更時 `reload_agent`）、各ターンでは
//! `provider_for` が既存接続を包んだ軽量な [`McpToolProvider`] を返す（ターン毎に
//! subprocess を起こさない）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tracing::{info, warn};

use opencrab_db::Db;

use crate::actions::{ConnectedServer, McpToolProvider};
use crate::client::McpClient;
use crate::config::{config_from_row, McpServerConfig};

/// per-agent の reload 調停。`latest` は「最後に要求された世代」、`lock` は per-agent の
/// 直列化用。要求は同期的（リクエスト順）に `latest` を進め、実行タスクは自分の世代が
/// まだ最新のときだけ再接続する（連続編集のコアレッシング＝古い設定が勝つ競合と
/// subprocess の同時多発を防ぐ）。
#[derive(Clone)]
struct ReloadCtl {
    latest: u64,
    lock: Arc<tokio::sync::Mutex<()>>,
}

pub struct McpClientManager {
    db: Db,
    // agent_id → 接続済みサーバ群。ガードは await を跨がない。
    agents: RwLock<HashMap<String, Vec<ConnectedServer>>>,
    // agent_id → (server_name → 直近の接続失敗理由)。接続成功したサーバは載らない。
    // operator/agent が「なぜ繋がらないか」を見られるよう surface する。
    connect_errors: RwLock<HashMap<String, HashMap<String, String>>>,
    reload_gen: AtomicU64,
    reload_ctl: std::sync::Mutex<HashMap<String, ReloadCtl>>,
}

impl McpClientManager {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            agents: RwLock::new(HashMap::new()),
            connect_errors: RwLock::new(HashMap::new()),
            reload_gen: AtomicU64::new(0),
            reload_ctl: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 指定エージェントの直近の接続失敗理由（server_name → error）。接続に成功した
    /// サーバは含まれない。
    pub fn connect_errors(&self, agent_id: &str) -> HashMap<String, String> {
        self.connect_errors
            .read()
            .unwrap()
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }

    /// reload 要求を登録する（**同期・リクエスト順**）。返した世代を [`run_reload`] に渡す。
    /// 直後に spawn する呼び出し側が使う。
    pub fn mark_reload_requested(&self, agent_id: &str) -> u64 {
        let gen = self.reload_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let mut ctl = self.reload_ctl.lock().unwrap();
        let entry = ctl
            .entry(agent_id.to_string())
            .or_insert_with(|| ReloadCtl {
                latest: gen,
                lock: Arc::new(tokio::sync::Mutex::new(())),
            });
        entry.latest = gen;
        gen
    }

    /// [`mark_reload_requested`] で得た世代で再接続する。per-agent 直列化し、自分が
    /// もう最新世代でなければ（より新しい要求が来ている）何もしない＝連続編集を畳む。
    pub async fn run_reload(&self, agent_id: &str, gen: u64) {
        let lock = {
            let ctl = self.reload_ctl.lock().unwrap();
            match ctl.get(agent_id) {
                Some(c) => c.lock.clone(),
                None => return,
            }
        };
        let _guard = lock.lock().await;
        // 自分が最新でなければ、後続の要求が（今か直後に）反映するのでスキップ。
        if self
            .reload_ctl
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|c| c.latest)
            .unwrap_or(0)
            != gen
        {
            return;
        }
        let configs = self.agent_configs(agent_id);
        self.start_agent(agent_id, configs).await;
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
        // エージェント間も並列に接続する（1 エージェントの hang が起動全体を止めない）。
        let starts = by_agent
            .into_iter()
            .map(|(agent_id, configs)| async move { self.start_agent(&agent_id, configs).await });
        futures::future::join_all(starts).await;
    }

    /// 指定エージェントのサーバ群へ接続する（既存接続は置き換え）。接続失敗した
    /// サーバはスキップして他を活かす（fail-soft）。
    pub async fn start_agent(&self, agent_id: &str, configs: Vec<McpServerConfig>) {
        // enabled のサーバへ**並列**に接続する。逐次だと 1 台の initialize hang が
        // 他サーバ（と、restore 時は他エージェント）の接続やサーバ起動を最大 30s/台
        // 待たせるため、join_all で同時に張る。
        let futs = configs
            .into_iter()
            .filter(|c| c.enabled)
            .map(|cfg| async move {
                let r = McpClient::connect(&cfg).await;
                (cfg, r)
            });
        let results = futures::future::join_all(futs).await;

        let mut connected = Vec::new();
        let mut errors: HashMap<String, String> = HashMap::new();
        for (cfg, r) in results {
            match r {
                Ok(client) => {
                    info!(agent_id, server = %cfg.name, tools = client.tools().len(), "MCP サーバ接続");
                    connected.push(ConnectedServer {
                        server: Arc::new(client),
                        trusted_only: cfg.trusted_only,
                    });
                }
                Err(e) => {
                    warn!(agent_id, server = %cfg.name, error = %e, "MCP サーバ接続に失敗（このサーバはスキップ）");
                    // operator/agent が診断できるよう理由を保持する。
                    errors.insert(cfg.name.clone(), e.to_string());
                }
            }
        }
        // 旧接続は drop で kill される。
        self.agents
            .write()
            .unwrap()
            .insert(agent_id.to_string(), connected);
        // このエージェントの失敗理由を最新の start 結果で置き換える（成功したものは消える）。
        self.connect_errors
            .write()
            .unwrap()
            .insert(agent_id.to_string(), errors);
    }

    /// DB から該当エージェントの設定を読み直して再接続する（インライン用の便宜メソッド）。
    /// spawn して使う場合は `mark_reload_requested`（同期）→ `run_reload` を使うこと
    /// （リクエスト順を保つため）。
    pub async fn reload_agent(&self, agent_id: &str) {
        let gen = self.mark_reload_requested(agent_id);
        self.run_reload(agent_id, gen).await;
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
        // 死んだ接続（サーバがクラッシュ/終了）は除外する。使えないツールを LLM に
        // 出し続けない（`reconnect_dead` の自己修復までの間の一貫性）。
        let servers: Vec<ConnectedServer> = self
            .agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|v| v.iter().filter(|s| s.server.is_alive()).cloned().collect())
            .unwrap_or_default();
        McpToolProvider::new(servers, caller_is_trusted)
    }

    /// 接続済み（かつ生存中）サーバの (name, tools 数) 一覧（ダッシュボード表示用）。
    pub fn connected_status(&self, agent_id: &str) -> Vec<(String, usize)> {
        self.agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|v| {
                v.iter()
                    .filter(|s| s.server.is_alive())
                    .map(|s| (s.server.server_name().to_string(), s.server.tools().len()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 何らかの生存中サーバに接続済みか。
    pub fn has_connections(&self, agent_id: &str) -> bool {
        self.agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|v| v.iter().any(|s| s.server.is_alive()))
            .unwrap_or(false)
    }

    /// 切断されたサーバ（クラッシュ/終了）を抱えるエージェントを再接続する（自己修復）。
    /// dead が無ければ何もしない。起動時に spawn した周期スイープから呼ぶ。
    pub async fn reconnect_dead(&self) {
        // dead を持つエージェントを収集（ロックは同期・await を跨がない）。
        let stale: Vec<String> = {
            let map = self.agents.read().unwrap();
            map.iter()
                .filter(|(_, servers)| servers.iter().any(|s| !s.server.is_alive()))
                .map(|(id, _)| id.clone())
                .collect()
        };
        for agent_id in stale {
            warn!(agent_id, "MCP: 切断されたサーバを検出、再接続します");
            let configs = self.agent_configs(&agent_id);
            self.start_agent(&agent_id, configs).await;
        }
    }

    pub fn shutdown_all(&self) {
        self.agents.write().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mark_reload_requested_monotonic() {
        let m = McpClientManager::new(opencrab_db::Db::memory().unwrap());
        let a = m.mark_reload_requested("agent-1");
        let b = m.mark_reload_requested("agent-1");
        assert!(b > a);
        // agent 間でも単調（グローバル世代）。
        let c = m.mark_reload_requested("agent-2");
        assert!(c > b);
    }

    #[tokio::test]
    async fn test_run_reload_skips_stale_generation() {
        let m = McpClientManager::new(opencrab_db::Db::memory().unwrap());
        // 古い要求 → 直後に新しい要求で latest を進める。
        let stale = m.mark_reload_requested("agent-1");
        let _newer = m.mark_reload_requested("agent-1");
        // 古い世代で run_reload しても、latest ではないので何もしない
        // （agent_configs/start_agent に到達せず＝接続を張らない）。DB は空なので
        //   仮に到達しても servers は空になるが、ここでは「未接続のまま」を確認する。
        m.run_reload("agent-1", stale).await;
        assert!(!m.has_connections("agent-1"));
        // 未登録の world で run_reload しても panic しない（ctl が無ければ即 return）。
        m.run_reload("no-such-agent", 999).await;
    }

    #[tokio::test]
    async fn connect_error_is_surfaced_on_failure() {
        let m = McpClientManager::new(opencrab_db::Db::memory().unwrap());
        // 存在しないコマンド → 接続失敗。理由が connect_errors に surface される。
        let cfg = McpServerConfig {
            name: "bad".to_string(),
            command: "/nonexistent/definitely-not-a-real-binary".to_string(),
            args: vec![],
            env: Default::default(),
            trusted_only: false,
            enabled: true,
        };
        m.start_agent("agent-x", vec![cfg]).await;
        assert!(!m.has_connections("agent-x"));
        let errs = m.connect_errors("agent-x");
        assert!(errs.contains_key("bad"), "failure reason must be surfaced");
        // 成功時（設定なし）は理由が消える。
        m.start_agent("agent-x", vec![]).await;
        assert!(m.connect_errors("agent-x").is_empty());
    }
}
