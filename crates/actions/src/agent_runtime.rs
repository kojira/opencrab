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
    ///
    /// `caller` は本ターンの呼び出し元。caller=Agent のときだけ skill index を露出許可
    /// （`agent_visible`）のものへ絞る（#352）。ここへ渡す caller は、同じターンの
    /// [`RunRequest`] に載せる caller と一致させること（index と実行権限を揃える）。
    fn build_agent_context(
        &self,
        agent_id: &str,
        caller: &crate::CallerIdentity,
    ) -> (String, String);

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

    /// プロセス全体で 1 つの per-session 直列化ロック表（#588 Stage 2）。
    ///
    /// 時間トリガー（heartbeat）のターンと通常メッセージ処理のターンを**同一セッション上で
    /// 直列化**するため、両者が同じ `SessionLocks` インスタンスを見る必要がある。各ゲートウェイの
    /// 受信ループ（Discord）や per-session ランタイム（Nostr の [`crate::SessionRuntime`]）は、
    /// ローカルに `SessionLocks::new()` を作るのをやめ、ここが返す共有インスタンスを使う。
    /// `AppState` が 1 つだけ生成して保持し、全経路がその `Arc` を clone して共有する。
    fn session_locks(&self) -> std::sync::Arc<crate::SessionLocks>;

    /// NO_REPLY（沈黙の明示）を記録する（best-effort）。
    fn record_agent_no_reply(&self, agent_id: &str, session_id: &str);

    /// ゲートウェイから受信した発言をセッションログへ記録する。**記録できたら `true`**。
    ///
    /// 由来（`metadata_json` の `source`）は [`TranscriptSource`] で受ける。行の形は
    /// `crates/server` の transcript モジュールが所有し、移設前とバイト等価な
    /// `metadata_json` を書く（#158 S3）。
    ///
    /// 他の転記メソッドと違い **best-effort ではない**（#284 P0-3）。ユーザー発言が
    /// 記録されないと、その発言は会話履歴に一切現れず、エージェントは**見ないまま**
    /// 応答する（＝対話が成立しない）。実装はリトライした上で、それでも駄目なら
    /// `false` を返すこと。呼び出し側は `false` を無視せずエスカレーションする。
    #[must_use]
    fn record_inbound_message(
        &self,
        source: TranscriptSource,
        record: &InboundMessageRecord<'_>,
    ) -> bool;

    /// 受信メッセージを**汎用層の受信処理**へ通す共通フック（#156 S4、best-effort）。
    ///
    /// 記録（[`Self::record_inbound_message`]）とは別で、「受信をきっかけに汎用層が
    /// 走らせたい副作用」の唯一の入口。現在の購読者は**ピアレビュー返信の回収**
    /// （`[Peer Review]` の解析 → タスク台帳への記録 / #58）1 つで、以前は Discord の
    /// 受信ループ 1 箇所からしか呼ばれていなかった（＝ Discord 経由の会話でしか
    /// レビューが回収されない）。フックをここへ置くことで、経路を足すたびに回収コードを
    /// コピーせずに済む。
    ///
    /// 新しい抽象を作らず既存の [`AgentRuntime`] に足しているのは、これが**すでに
    /// 3 ゲートウェイ共通の境界**（各ゲートウェイの runner トレイトのスーパートレイト）
    /// であり、実装が `AppState` 1 つに閉じているため。受信フック専用のトレイトや
    /// 登録簿を新設しても、実装 1・購読者 1 の飾りが増えるだけになる。
    ///
    /// 引数は [`Self::record_inbound_message`] と同じ [`InboundMessageRecord`] に
    /// **受信側エージェントの id** を添えた形（record 側は発言の帰属＝送信者しか
    /// 持たないが、回収は「誰の台帳か」を必要とする）。
    ///
    /// 呼ぶ位置は**会話文字列を組み立てる前**（`[Task Ledger]` は会話の先頭に前置される
    /// ため、後から呼ぶと回収した verdict がそのターンに現れない）。
    ///
    /// 実装は best-effort（失敗しても受信処理を止めない）。
    fn on_inbound_message(
        &self,
        source: TranscriptSource,
        agent_id: &str,
        record: &InboundMessageRecord<'_>,
    );

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
