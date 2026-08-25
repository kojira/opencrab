//! web ゲートウェイがエージェント実行・永続化に必要とする最小 runner 境界（#190 S3）。
//!
//! `crates/server` の `AppState` が実装し、既存の process / transcript / queries
//! ヘルパへ委譲する（`crates/server/src/web_runner_impl.rs`）。
//!
//! ## 設計の制約
//!
//! - **DB 行の型を出さない**。トレイトは「会話履歴の文字列」「認可判定の結果
//!   （[`CallerIdentity`]）」「保存の成否」だけを扱う。DB スキーマ（`SessionRow` /
//!   `SessionLogRow` / `AgentDiscordConfigRow` など）はこのクレートに現れないため、
//!   カラム追加や行構造の変更がゲートウェイ層へ波及しない。
//!   （Nostr の `NostrAgentRunner` は設定行の型を露出しているが、それは踏襲しない。）
//! - **失敗の表現は `anyhow::Result`**。HTTP レスポンスの文面は呼び出し側が組む
//!   （実装は DB エラーをそのまま伝播させ、既存のメッセージを保つ）。
//! - `Clone + Send + Sync + 'static`: 完了 sink は `tokio::spawn` の中で resume する
//!   ため、runner を所有して move できる必要がある（境界は [`AgentRuntime`] が持つ）。

use std::sync::Arc;

use anyhow::Result;

use opencrab_actions::{AgentRuntime, CallerIdentity, InboundIdentity, SessionInboundWrite};

use crate::gateway::WebGateway;

/// web inbound は DM ではない。[`plan_inbound`](opencrab_actions::plan_inbound) は
/// `guild_id` が空のときだけ DM ゲートを見るので、ここは非空の番兵を渡す。
pub const WEB_INBOUND_GUILD: &str = "web";

/// [`InboundIdentity`] の web 側。
///
/// `AppState` の `InboundIdentity` は Discord 経路固定（trusted_users の platform）。
/// web は同じ [`plan_inbound`](opencrab_actions::plan_inbound) に、このアダプタを渡す
/// （web 専用の別 inbound 口は作らない）。計算本体は [`WebAgentRunner::resolve_caller`]。
///
/// web に DM / チャンネル whitelist は無い。現行どおり全員がターンに入り、
/// 権限の差は caller の variant だけ。
pub struct WebInboundIdentity<'a, R: WebAgentRunner> {
    runner: &'a R,
}

impl<'a, R: WebAgentRunner> WebInboundIdentity<'a, R> {
    pub fn new(runner: &'a R) -> Self {
        Self { runner }
    }
}

impl<R: WebAgentRunner> InboundIdentity for WebInboundIdentity<'_, R> {
    fn resolve_caller(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        _owner_id: &str,
    ) -> CallerIdentity {
        assert_eq!(
            agent_ids.len(),
            1,
            "web inbound は path の agent 1 体（複数 agent の別経路を作らない）"
        );
        self.runner.resolve_caller(&agent_ids[0], sender_id)
    }

    fn dm_allowed_any(&self, _sender_id: &str, _agent_ids: &[String], _owner_id: &str) -> bool {
        true
    }

    fn dm_allowed(&self, _sender_id: &str, _agent_id: &str, _owner_id: &str) -> bool {
        true
    }

    fn is_channel_whitelisted_for_agent(&self, _channel_id: &str, _agent_id: &str) -> bool {
        true
    }
}

/// ゲートウェイ非依存な実行・セッション管理は [`AgentRuntime`] が持つ（#156 S1）。
/// ここには web の語彙（ダッシュボードのユーザ、SSE ランタイム、web セッションの形）を
/// 含むものだけを宣言する。
pub trait WebAgentRunner: AgentRuntime {
    /// 呼び出し元の権限を導出する（trusted_users / owner 設定。計算本体）。
    ///
    /// ゲート（HTTP ハンドラ）は呼ばない。[`plan_inbound`](opencrab_actions::plan_inbound) が
    /// [`WebInboundIdentity`] 越しに呼ぶ。返すのは権限モデルの型だけで、判定に使う
    /// 設定行はここには出さない。
    fn resolve_caller(&self, agent_id: &str, user_id: &str) -> CallerIdentity;

    /// web 会話セッションが無ければ作成する。
    ///
    /// [`AgentRuntime::ensure_session`] とは別物: web のセッションは mode/phase/theme が
    /// 固定で metadata を持たず、失敗を呼び出し側へ返す（best-effort ではない）。
    fn ensure_web_session(&self, session_id: &str, agent_id: &str) -> Result<()>;

    /// ダッシュボードからのユーザ発話をセッションログへ記録する。
    ///
    /// 応答生成は DB から会話を再構築するため、この記録が先に成功している必要がある
    /// （失敗したら実行せずにエラーを返す）。
    fn record_user_message(
        &self,
        agent_id: &str,
        session_id: &str,
        user_id: &str,
        content: &str,
    ) -> Result<()>;

    /// エージェントの応答をセッションログへ記録する（best-effort）。
    fn record_agent_reply(
        &self,
        agent_id: &str,
        session_id: &str,
        text: &str,
        iterations: usize,
        tool_calls_made: usize,
    );

    /// web ゲートウェイの共有ランタイム（SSE チャンネル + per-session 直列化）。
    ///
    /// プロセス全体で 1 つの `Arc` を返すこと。inbound と完了 sink が**同じ**ランタイムに
    /// 到達することが、直列化（二重回答の防止）と registry 共有（`cancel_subtask` の
    /// 到達性）の前提である。runner から引く形にしておけば、呼び出し側が別のランタイムを
    /// 渡してしまう余地が無くなる。
    fn web_gateway(&self) -> &Arc<WebGateway>;
}

/// [`SessionInboundWrite`] の web 側。行の形は [`WebAgentRunner`] のまま。
pub struct WebSessionWrite<'a, R: WebAgentRunner>(&'a R);

impl<'a, R: WebAgentRunner> WebSessionWrite<'a, R> {
    pub fn new(runner: &'a R) -> Self {
        Self(runner)
    }
}

impl<R: WebAgentRunner> SessionInboundWrite for WebSessionWrite<'_, R> {
    fn ensure_web_session(&self, session_id: &str, agent_id: &str) -> Result<()> {
        self.0.ensure_web_session(session_id, agent_id)
    }

    fn record_user_message(
        &self,
        agent_id: &str,
        session_id: &str,
        user_id: &str,
        content: &str,
    ) -> Result<()> {
        self.0
            .record_user_message(agent_id, session_id, user_id, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::{plan_inbound, NormalizedInboundEvent};

    use crate::testing::FakeRunner;

    /// 同じ core 入り口。web 専用の admit 口を作らず、`plan_inbound` が caller を決める。
    #[test]
    fn plan_inbound_admits_web_and_resolves_caller() {
        let runner = FakeRunner::new("ok").with_caller(CallerIdentity::Owner);
        let event = NormalizedInboundEvent {
            sender_id: "alice",
            channel_id: "",
            guild_id: WEB_INBOUND_GUILD,
        };
        let plan = plan_inbound(&WebInboundIdentity::new(&runner), &event, "", &["a".into()])
            .expect("web inbound は DM ではない");
        assert_eq!(plan.caller, CallerIdentity::Owner);
        assert_eq!(plan.admitted_agent_ids, vec!["a".to_string()]);
        assert!(plan.agent_drops.is_empty());
        let lookups = runner.caller_lookups();
        assert_eq!(lookups.len(), 1);
        assert_eq!(lookups[0].agent_id, "a");
        assert_eq!(lookups[0].user_id, "alice");
    }
}
