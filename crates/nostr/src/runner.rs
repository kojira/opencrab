//! nostr ゲートウェイがエージェント実行に必要とする最小 runner 境界。
//!
//! discord の `AgentRunner`（巨大・Discord 固有）に依存しないよう、必要なメソッド
//! だけを切り出したトレイト。`crates/server` の `AppState` が実装し、既存の
//! `run_agent_response` / セッション/転記ヘルパへ委譲する。

use anyhow::Result;
use async_trait::async_trait;

use opencrab_actions::RunRequest;
use opencrab_core::EngineResult;
use opencrab_db::queries::AgentNostrConfigRow;

#[async_trait]
pub trait NostrAgentRunner: Send + Sync + Clone + 'static {
    /// エージェント応答パイプライン（SkillEngine + LLM）を実行する。
    async fn run_agent_response(&self, req: RunRequest) -> Result<EngineResult>;

    /// system prompt と表示名を組み立てる（`(system_prompt, agent_name)`）。
    fn build_agent_context(&self, agent_id: &str) -> (String, String);

    /// セッションの会話履歴文字列（コンパクション込み）を組み立てる。
    fn build_conversation_string(
        &self,
        session_id: &str,
        agent_id: &str,
        context_budget_tokens: usize,
    ) -> Result<String>;

    /// 会話コンテキストのトークン予算。
    fn context_budget_tokens(&self, agent_id: &str) -> usize;

    /// セッションが無ければ作成する（best-effort）。
    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
    );

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
