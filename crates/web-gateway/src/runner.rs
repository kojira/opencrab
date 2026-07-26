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
//!   ため、runner を所有して move できる必要がある。

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_core::EngineResult;

use crate::gateway::WebGateway;

#[async_trait]
pub trait WebAgentRunner: Send + Sync + Clone + 'static {
    /// エージェント応答パイプライン（SkillEngine + LLM）を実行する。
    async fn run_agent_response(&self, req: RunRequest) -> Result<EngineResult>;

    /// system prompt と表示名を組み立てる（`(system_prompt, agent_name)`）。
    fn build_agent_context(&self, agent_id: &str) -> (String, String);

    /// セッションの会話履歴文字列（コンパクション込み）を組み立てる。
    ///
    /// 二重回答を防ぐ要: resume は完了本文を sink で運ばず、DB から会話を再構築する。
    fn build_conversation_string(
        &self,
        session_id: &str,
        agent_id: &str,
        context_budget_tokens: usize,
    ) -> Result<String>;

    /// 会話コンテキストのトークン予算（有効モデルの context window × 比率）。
    fn context_budget_tokens(&self, agent_id: &str) -> usize;

    /// LLM プロバイダが 1 つ以上使えるか（未設定なら実行せずに返す）。
    fn has_llm_provider(&self) -> bool;

    /// 呼び出し元の権限を判定する（trusted_users / owner 設定から導出）。
    ///
    /// 返すのは権限モデルの型だけで、判定に使う設定行はここには出さない。
    fn resolve_caller(&self, agent_id: &str, user_id: &str) -> CallerIdentity;

    /// web 会話セッションが無ければ作成する。
    fn ensure_session(&self, session_id: &str, agent_id: &str) -> Result<()>;

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
