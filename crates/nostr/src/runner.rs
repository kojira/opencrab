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
    // 転記（受信イベント / エージェント返信）は [`AgentRuntime`] が持つ（#158 S3）。
    // `record_inbound_message` / `record_outbound_reply` を
    // `TranscriptSource::Nostr` で呼ぶ。Discord と行の形が同じなので、gateway ごとに
    // 宣言を分ける理由が無い。

    /// enabled な per-agent Nostr 設定一覧（起動時 restore 用）。
    fn list_enabled_nostr_configs(&self) -> Vec<AgentNostrConfigRow>;

    /// エージェントの Nostr 設定行を取得する（identity 切替で relays 継承に使う）。
    fn get_nostr_config(&self, agent_id: &str) -> Option<AgentNostrConfigRow>;

    /// 本鍵（secret_key）だけを差し替える（identity 切替。relays/filter/enabled は保持）。
    fn set_nostr_secret_key(&self, agent_id: &str, secret_key: &str) -> Result<()>;

    /// エージェント宛の Nostr 受信を転記する宛先を解決する（issue #252 段階 A）。
    ///
    /// エージェント単位設定（`agent_nostr_relay_config`）を**同期 DB 読み**で引き、有効かつ
    /// 宛先が妥当なときだけ [`WebhookConfig`] を返す。未設定 / 無効 / 不正はすべて `None`
    /// （fail-closed = 転記しない）。受信ループから直接呼ぶので、実装は軽い読み 1 回に留め、
    /// await しない。
    ///
    /// 戻り値は actions 層の gateway 非依存な [`WebhookConfig`] で、Nostr crate は Discord
    /// 固有の型に触れない（#191 の筋 / issue #252 の層制約）。
    ///
    /// [`WebhookConfig`]: opencrab_actions::webhook_target::WebhookConfig
    fn resolve_nostr_relay_target(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_actions::webhook_target::WebhookConfig>;

    /// 解決済みの宛先へ 1 件の転記本文を**非ブロック**で送る（issue #252 段階 A）。
    ///
    /// 送信は実装側で fire-and-forget（受信ループを止めない）。送信失敗は**ログのみ**で、
    /// 応答生成や他セッションの受信を巻き込まない。宛先型は actions 層の共通口
    /// （[`WebhookConfig`]）で、Nostr crate は Discord を名指ししない。
    ///
    /// [`WebhookConfig`]: opencrab_actions::webhook_target::WebhookConfig
    fn relay_inbound_notification(
        &self,
        target: &opencrab_actions::webhook_target::WebhookConfig,
        text: String,
    );
}
