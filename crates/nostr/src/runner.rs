//! nostr ゲートウェイがエージェント実行に必要とする最小 runner 境界。
//!
//! discord の `AgentRunner`（巨大・Discord 固有）に依存しないよう、必要なメソッド
//! だけを切り出したトレイト。`crates/server` の `AppState` が実装し、既存の
//! `run_agent_response` / セッション/転記ヘルパへ委譲する。
//!
//! ゲートウェイ非依存な実行・セッション管理は [`opencrab_actions::AgentRuntime`] が
//! 持つ（#156 S1）。ここには Nostr の語彙（イベント・pubkey・設定行）を含むものだけを
//! 宣言する。

use anyhow::Result;

use opencrab_actions::AgentRuntime;
use opencrab_db::queries::AgentNostrConfigRow;

pub trait NostrAgentRunner: AgentRuntime {
    /// 受信 Nostr イベントをセッションログに残す（best-effort）。
    fn record_nostr_user_message(
        &self,
        session_id: &str,
        sender_pubkey: &str,
        sender_name: &str,
        text: &str,
    );

    /// エージェントの Nostr 返信をセッションログに残す（best-effort）。
    fn record_nostr_agent_reply(&self, agent_id: &str, session_id: &str, text: &str);

    /// enabled な per-agent Nostr 設定一覧（起動時 restore 用）。
    fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow>;

    /// エージェントの Nostr 設定行を取得する（identity 切替で relays 継承に使う）。
    fn get_nostr_config(&self, agent_id: &str) -> Option<AgentNostrConfigRow>;

    /// 本鍵（secret_key）だけを差し替える（identity 切替。relays/filter/enabled は保持）。
    fn set_nostr_secret_key(&self, agent_id: &str, secret_key: &str) -> Result<()>;
}
