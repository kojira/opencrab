//! identity 切替（生成した vanity 鍵を本鍵として採用）の境界トレイト。
//!
//! 実体は watch ループ側（`manager.rs`）が握る（runner+cli+self_pubkey セルを capture）。
//! `NostrGatewayActions` は trait object として保持し、`nostr_switch_identity`
//! （owner/trusted 限定）ツールから呼ぶ。

use async_trait::async_trait;

/// 生成鍵をエージェントの本鍵に採用する管理操作。
#[async_trait]
pub trait NostrIdentityAdmin: Send + Sync {
    /// `generated-keys/<npub>.nsec` の鍵を本鍵に採用する。
    ///
    /// DB の secret_key を更新し、config.toml を新鍵で再生成（0600）し、自己返信スキップ用の
    /// self_pubkey を新 pubkey へ更新する。legacy は watch 無停止。v3 は停止→revision→再起動。
    /// 成功時は採用した npub を返す。秘密鍵は返さない。
    async fn adopt_generated_identity(&self, agent_id: &str, npub: &str) -> anyhow::Result<String>;
}
