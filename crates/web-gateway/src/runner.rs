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

use opencrab_actions::{AgentRuntime, CallerIdentity};

use crate::gateway::WebGateway;

/// web inbound は DM ではない。[`accept_inbound`](opencrab_actions::accept_inbound) は
/// `guild_id` が空のときだけ DM ゲートを見るので、ここは非空の番兵を渡す。
pub const WEB_INBOUND_GUILD: &str = "web";

/// ゲートウェイ非依存な実行・セッション管理は [`AgentRuntime`] が持つ（#156 S1）。
/// ここには web の語彙（ダッシュボードのユーザ、SSE ランタイム、web セッションの形）を
/// 含むものだけを宣言する。
pub trait WebAgentRunner: AgentRuntime {
    /// 呼び出し元の権限を導出する（trusted_users / owner 設定。計算本体）。
    ///
    /// ゲート（HTTP ハンドラ）は呼ばない。[`accept_inbound`](opencrab_actions::accept_inbound) が
    /// 呼ぶ。返すのは権限モデルの型だけで、判定に使う設定行はここには出さない。
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

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::{accept_inbound, InboundLookups, InboundWork, NormalizedInboundEvent};

    use crate::testing::FakeRunner;

    /// 同じ core 入り口。web 専用の admit 口を作らず、`accept_inbound` が caller を決める。
    #[test]
    fn accept_inbound_admits_web_and_resolves_caller() {
        let runner = FakeRunner::new("ok").with_caller(CallerIdentity::Owner);
        let event = NormalizedInboundEvent {
            sender_id: "alice",
            channel_id: "",
            guild_id: WEB_INBOUND_GUILD,
        };
        let work = InboundWork {
            event,
            has_content: true,
            kind_label: "",
            author_key: "alice",
        };
        let resolve = |s: &str, a: &[String], _: &str| runner.resolve_caller(&a[0], s);
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let mut caller = None;
        let mut admitted = Vec::new();
        accept_inbound::<()>(
            &[work],
            "",
            &["a".into()],
            &lookups,
            None,
            |_| (),
            |_, adm| {
                caller = Some(adm.caller.clone());
                admitted = adm.admitted_agent_ids.clone();
            },
            |_, _| {},
        )
        .expect("web inbound は DM ではない");
        assert_eq!(caller, Some(CallerIdentity::Owner));
        assert_eq!(admitted, vec!["a".to_string()]);
        let looked = runner.caller_lookups();
        assert_eq!(looked.len(), 1);
        assert_eq!(looked[0].agent_id, "a");
        assert_eq!(looked[0].user_id, "alice");
    }
}
