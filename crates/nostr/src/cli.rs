//! nostaro（自作 Nostr CLI）を subprocess 制御するラッパー。
//!
//! codex/cursor プロバイダと同じ「別コマンドを spawn して制御」パターン。鍵の共有
//! 事故を防ぐため、エージェント毎に **一意な config パス**（`data/agents/{id}/nostr/
//! config.toml`）を `--config` で明示指定する（`resolve_agent_workspace` と同じ検証
//! 経路で組む）。リレー/フィルタは watch のフラグで渡し、nostaro の config 側 default
//! に依存しない（指定リレー以外に繋がせない）。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use opencrab_core::workspace::resolve_agent_workspace;
use tokio::process::Command;

use crate::config::NostrConfig;

const DEFAULT_NOSTARO_PATH: &str = "nostaro";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// nostaro CLI ラッパー。
#[derive(Debug, Clone)]
pub struct NostaroCli {
    binary_path: String,
    timeout: Duration,
}

impl Default for NostaroCli {
    fn default() -> Self {
        Self::new()
    }
}

impl NostaroCli {
    pub fn new() -> Self {
        Self {
            binary_path: DEFAULT_NOSTARO_PATH.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    pub fn with_binary_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        if !p.trim().is_empty() {
            self.binary_path = p;
        }
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.timeout = Duration::from_secs(secs);
        }
        self
    }

    /// エージェント毎の Nostr 専用ディレクトリ（鍵・config の隔離先）。
    /// `validate_agent_id` を通す唯一の入口（パストラバーサル防止）。
    pub fn agent_nostr_dir(agent_id: &str) -> Result<PathBuf> {
        resolve_agent_workspace("data/agents/{agent_id}/nostr", agent_id)
    }

    /// エージェント毎の nostaro config パス（`--config` に渡す）。
    pub fn agent_config_path(agent_id: &str) -> Result<PathBuf> {
        Ok(Self::agent_nostr_dir(agent_id)?.join("config.toml"))
    }

    /// 共通の base command（`nostaro --config <per-agent> <subcommand>...`）。
    fn base_command(&self, agent_id: &str) -> Result<Command> {
        let config_path = Self::agent_config_path(agent_id)?;
        // 親ディレクトリを用意（鍵 config の置き場所）。
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cmd = Command::new(&self.binary_path);
        cmd.kill_on_drop(true);
        cmd.arg("--config").arg(&config_path);
        Ok(cmd)
    }

    /// 一発実行系（post/reply/dm/zap/upload）を timeout 付きで走らせ stdout を返す。
    async fn run(&self, mut cmd: Command) -> Result<String> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("nostaro timed out after {}s", self.timeout.as_secs()))?
            .context("failed to run nostaro")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("nostaro failed ({}): {}", output.status, stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// `nostaro post "<text>"` — 新規ノート投稿。
    pub async fn post(&self, agent_id: &str, text: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("post").arg(text);
        self.run(cmd).await
    }

    /// `nostaro reply <target> "<text>"` — 返信。target は note1.../hex id。
    pub async fn reply(&self, agent_id: &str, target: &str, text: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("reply").arg(target).arg(text);
        self.run(cmd).await
    }

    /// `nostaro dm send <recipient> "<text>"`（既定 NIP-17）。
    pub async fn dm(&self, agent_id: &str, recipient: &str, text: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("dm").arg("send").arg(recipient).arg(text);
        self.run(cmd).await
    }

    /// `nostaro zap <recipient> <amount> -m "<message>"`。
    pub async fn zap(
        &self,
        agent_id: &str,
        recipient: &str,
        amount: u64,
        message: Option<&str>,
    ) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("zap").arg(recipient).arg(amount.to_string());
        if let Some(m) = message.filter(|s| !s.is_empty()) {
            cmd.arg("-m").arg(m);
        }
        self.run(cmd).await
    }

    /// `nostaro upload <path>` — Blossom アップロード。返り値は URL。
    pub async fn upload(&self, agent_id: &str, path: &str) -> Result<String> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("upload").arg(path);
        self.run(cmd).await
    }

    /// watch 用の Command を組む（spawn はループ側が行い、stdout の JSONL を読む）。
    ///
    /// リレー/フィルタは**必ずフラグで明示**して渡す（config の default に依存しない
    /// ＝指定リレー以外へ繋がせない）。`--json` で JSONL を stdout に出させる。
    pub fn build_watch_command(&self, agent_id: &str, config: &NostrConfig) -> Result<Command> {
        let mut cmd = self.base_command(agent_id)?;
        cmd.arg("watch").arg("--json");
        for relay in config.effective_relays() {
            cmd.arg("--relay").arg(relay);
        }
        for author in &config.filter.authors {
            cmd.arg("--author").arg(author);
        }
        for keyword in &config.filter.keywords {
            cmd.arg("--keyword").arg(keyword);
        }
        for kind in config.effective_kinds() {
            cmd.arg("--kind").arg(kind.to_string());
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_dir_isolated_per_agent() {
        let a = NostaroCli::agent_nostr_dir("agent-1").unwrap();
        let b = NostaroCli::agent_nostr_dir("agent-2").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("data/agents/agent-1/nostr"));
        assert!(NostaroCli::agent_config_path("agent-1")
            .unwrap()
            .ends_with("data/agents/agent-1/nostr/config.toml"));
    }

    #[test]
    fn test_agent_dir_rejects_traversal_id() {
        // validate_agent_id 経由なので `../` 入りは弾かれる。
        assert!(NostaroCli::agent_nostr_dir("../etc").is_err());
        assert!(NostaroCli::agent_nostr_dir("a/b").is_err());
        assert!(NostaroCli::agent_nostr_dir("").is_err());
    }

    #[test]
    fn test_watch_command_includes_relays_and_filters() {
        let cli = NostaroCli::new();
        let config = NostrConfig {
            relays: vec![],
            filter: crate::config::NostrFilter {
                authors: vec!["npub1abc".to_string()],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        let cmd = cli.build_watch_command("agent-1", &config).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // 既定リレー2つがフラグで渡る（config の default に依存しない）。
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--relay" && w[1] == "wss://yabu.me"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--relay" && w[1] == "wss://r.kojira.io"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--author" && w[1] == "npub1abc"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--keyword" && w[1] == "opencrab"));
        // kind 未指定 → 既定 1。
        assert!(args.windows(2).any(|w| w[0] == "--kind" && w[1] == "1"));
        assert!(args.contains(&"--json".to_string()));
        // per-agent config が渡る。
        assert!(args
            .iter()
            .any(|a| a.contains("data/agents/agent-1/nostr/config.toml")));
    }
}
