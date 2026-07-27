//! ゲートウェイ非依存なエージェント実行境界（#156 S1）。
//!
//! discord / nostr / web の 3 つのゲートウェイは、それぞれ「サーバ側のエージェント実行
//! パイプライン」への境界トレイト（`AgentRunner` / `NostrAgentRunner` /
//! `WebAgentRunner`）を持っていた。そのうち**ゲートウェイの語彙をまったく含まない**
//! メソッド群（応答生成・会話履歴の組み立て・トークン予算・セッション/インタラクション
//! 管理）は 3 箇所でほぼ同一の宣言として重複しており、`crates/server` 側の実装も
//! 3 ファイルに同じ本体がコピーされていた。同じ契約が 3 箇所にあると片方だけ直る
//! （＝片方だけ壊れる）ため、ここ 1 つに寄せる。
//!
//! 各ゲートウェイのトレイトはこれを**スーパートレイト**として継承し、自分の語彙を
//! 含むメソッド（Discord のチャンネル判定、Nostr の設定行、web の SSE ランタイム等）
//! だけを宣言する。新しいゲートウェイを足すときは、この 10 メソッドは実装済みとして
//! 受け取れる。
//!
//! ここに置く理由（依存方向）: 依存は server → 各ゲートウェイであり、ゲートウェイ側から
//! server の型は参照できない。[`crate::session_runtime::SessionRuntime`] と同じ
//! gateway 非依存層に置くことで、どのゲートウェイからも同じ 1 つの契約を使える。
//!
//! **DB 行の型を出さない**という web 側の制約はここでも守る。会話は文字列、権限は
//! [`crate::CallerIdentity`]、失敗は `anyhow::Result` で表す。

use anyhow::Result;
use async_trait::async_trait;

use opencrab_core::EngineResult;

use crate::transcript::{
    InboundMessageRecord, InteractionRecord, OutboundReplyRecord, TranscriptSource,
};
use crate::RunRequest;

/// ゲートウェイ非依存なエージェント実行・セッション管理の境界。
///
/// `crates/server` の `AppState` が 1 箇所で実装する
/// （`crates/server/src/agent_runtime_impl.rs`）。
///
/// `Clone + Send + Sync + 'static`: 完了 sink は `tokio::spawn` の中で resume するため、
/// runner を所有して move できる必要がある。
#[async_trait]
pub trait AgentRuntime: Send + Sync + Clone + 'static {
    /// エージェント応答パイプライン（SkillEngine + LLM）を実行する。
    ///
    /// 実行要求は [`RunRequest`]（#33）で受ける。
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
    ///
    /// `agent_id` の per-agent モデルに応じた pricing を参照する。
    fn context_budget_tokens(&self, agent_id: &str) -> usize;

    /// LLM プロバイダが 1 つ以上使えるか（未設定なら実行せずに返す）。
    fn has_llm_providers(&self) -> bool;

    /// NO_REPLY（沈黙の明示）を記録する（best-effort）。
    fn record_agent_no_reply(&self, agent_id: &str, session_id: &str);

    /// ゲートウェイから受信した発言をセッションログへ記録する（best-effort）。
    ///
    /// 由来（`metadata_json` の `source`）は [`TranscriptSource`] で受ける。行の形は
    /// `crates/server` の transcript モジュールが所有し、移設前とバイト等価な
    /// `metadata_json` を書く（#158 S3）。
    fn record_inbound_message(&self, source: TranscriptSource, record: &InboundMessageRecord<'_>);

    /// エージェントの応答をセッションログへ記録する（best-effort）。
    ///
    /// このターンの起動要因は [`crate::AgentReplyContext`] で表す（Nostr のように
    /// 記録しない由来は `None`）。
    fn record_outbound_reply(&self, source: TranscriptSource, record: &OutboundReplyRecord<'_>);

    /// A2UI インタラクション応答をセッションログへ記録する（best-effort）。
    fn record_interaction_response(
        &self,
        agent_id: &str,
        session_id: &str,
        record: &InteractionRecord<'_>,
    );

    /// セッションが無ければ作成し、あれば metadata 未設定時のみ補完する（best-effort）。
    ///
    /// `mode` は作成するセッションの mode 列（`"discord"` / `"nostr"` 等）。どのゲートウェイ
    /// 由来の会話かは呼び出し側だけが知っているのでここで受け取る。既存セッションがある
    /// 場合は mode を書き換えない（従来の挙動）。
    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
        mode: &str,
    );

    /// セッションの theme を返す（無ければ None）。
    fn session_theme(&self, session_id: &str) -> Option<String>;

    /// pending interaction のステータスを更新する（best-effort）。
    fn mark_interaction_status(
        &self,
        interaction_id: &str,
        status: &str,
        response_json: Option<&str>,
        responder_id: Option<&str>,
    );

    /// 前プロセスから残った pending interaction を**期限切れとして閉じる**（プロセス起動時）。
    ///
    /// メモリ上の登録簿は再起動で消えるため、`pending` のまま残った行はどこへも応答を
    /// 返せない。無言で放置せず閉じ、何を閉じたかをログに残す（#196）。
    fn cleanup_stale_interactions(&self);

    /// 指定エージェント分だけ pending interaction を閉じる（per-agent ゲートウェイ起動時）。
    ///
    /// per-agent ゲートウェイは実行中にも再起動できるため、全件を閉じると同時に動いて
    /// いる**別エージェントの生きた保留対話**まで落としてしまう（#196）。
    fn cleanup_stale_interactions_for_agent(&self, agent_id: &str);
}
