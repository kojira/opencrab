//! Gateway 非依存の subtask 抽象（RFC #152 案A / S0）。
//!
//! この段（S0）では「まだ誰も使わない」抽象を追加するだけであり、既存の
//! Discord 実装（`crates/discord` の `SpawnedSubtask` / `SubtaskRegistry` /
//! `LoopEvent` 経由の完了通知）とは配線しない。S1 で Discord 側をこの抽象へ
//! 載せ替える。
//!
//! 設計の核（RFC §1.3・§3.1）:
//! - 完了通知に本文（result）は運搬しない。本文は既に session_logs（DB）へ
//!   永続化済みで、再注入は `build_conversation_string` が DB から会話を
//!   再構築する。sink に必要なのは「親セッションのエージェントを resume せよ」
//!   という**軽量トリガ**だけ。
//! - Discord 固有型（`WebhookConfig` / `DeliveryBatch` / serenity 等）は
//!   ここには一切入れない。webhook 系は S1 で discord 側の随伴構造へ分離する。

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::task::AbortHandle;

use crate::traits::CallerIdentity;

mod dispatcher;
mod manage;
mod settle;
mod sink;

pub use dispatcher::{default_non_dispatch_tools, SharedExecutor, SubtaskToolDispatcher};
pub use manage::{cancel_subtask, steer_subtask, CancelOutcome, SteerOutcome, STEER_LOG_TYPE};
pub use settle::{settle_completed, SettleContext};
pub use sink::{dispatch_settled, NoopCompletionSink, SubtaskCompletionSink, SubtaskSettled};

/// dispatch した subtask の既定タイムアウト（秒）。`spawn_subtask` の既定と揃える
/// （`crates/discord/src/gateway_actions/subtask_engine.rs` の `timeout_secs`）。
///
/// これが無いと、ハングするツール（応答しないネットワーク・終わらないコマンド）が
/// registry に永久滞留し、`exit_reason="timeout"` が到達不能になる（REST は
/// `sessions.status` が永久 `active`、依頼が無言で消える）。
pub const DEFAULT_DISPATCH_TIMEOUT_SECS: u64 = 1800;

/// subtask が settle（決着）したときの種別。
///
/// progress の二重定義を避け、完了と進捗の責務を `exit_reason` 文字列ではなく
/// 型で分ける（RFC レビュー指摘 P2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleKind {
    /// subtask 本体が終了した（completed / error / timeout / stopped_by_limit 等、
    /// 詳細は `SubtaskSettled::exit_reason`）。
    Completed,
    /// 走行中の中間進捗通知。
    Progress,
    /// `cancel_subtask` により停止した（完了ではない）。
    ///
    /// この種別は `SubtaskCompletionSink::on_subtask_cancelled` からのみ渡る。
    /// 完了経路（`on_subtask_settled`）とは別メソッドなので、resume する sink が
    /// 誤って「停止したのに返信する」ことはない。
    Cancelled,
}

// ---------------------------------------------------------------------------
// subtask のライフサイクル排他（cancel と settle の競合窓を閉じる）
// ---------------------------------------------------------------------------

const LIFECYCLE_RUNNING: u8 = 0;
const LIFECYCLE_CANCELLED: u8 = 1;
const LIFECYCLE_SETTLING: u8 = 2;

/// 「停止（cancel）」と「決着（settle）」のどちらが先に主張したかを 1 回だけ確定させる
/// 排他ラッチ。
///
/// これが無いと次の窓が空く: ツール本体が完走してから `settle_completed` が
/// DB 永続化を終えるまでの間（JSON 化 + DB ロック取得 + INSERT）に `cancel_subtask`
/// が入ると、`abort()` はもう効かず（await 後は同期実行）、cancel が成功を返した上で
/// `subtask_completed` が DB に書かれ sink が発火して**止めたのに返信が届く**。
///
/// `claim_cancel` / `claim_settle` は CAS（Running からの遷移）なので、両者が同時に
/// 走っても成功するのは一方だけ。cancel が勝てば settle は DB 記録も sink 発火も
/// 行わず、settle が勝てば cancel は「もう停止できない」ことを知れる。
#[derive(Debug, Clone)]
pub struct SubtaskLifecycle {
    state: Arc<AtomicU8>,
    /// 完走した call の部分結果（cancel 時に親ログへ残すため / #152 レビュー P2）。
    ///
    /// 1 バッチ = 1 subtask にしたことで粒度が粗くなり、`cancel_subtask` は
    /// `settle_completed` を丸ごと抑止するため「3 ファイル書いた後に止めた」場合に
    /// **どこまで進んだか**がラベルしか残らなかった。走行タスクが 1 call 完走ごとに
    /// ここへ積み、`cancel_subtask` が `tool_cancelled` の本文/メタデータへ載せる。
    ///
    /// ラッチと同じ「cancel 側（registry エントリ）と走行タスク側が共有する 1 個の
    /// ハンドル」なので、`SpawnedSubtask` に新フィールドを増やさずに共有できる
    /// （既存の全構築点は `SubtaskLifecycle::new()` を呼ぶだけで空の記録を得る）。
    completed_calls: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl Default for SubtaskLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl SubtaskLifecycle {
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(LIFECYCLE_RUNNING)),
            completed_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// 完走した 1 call の部分結果を記録する（cancel 時の「どこまで進んだか」用）。
    pub fn record_completed_call(&self, entry: serde_json::Value) {
        if let Ok(mut v) = self.completed_calls.lock() {
            v.push(entry);
        }
    }

    /// これまでに完走した call の部分結果（cancel 時に親ログへ載せる）。
    pub fn completed_calls(&self) -> Vec<serde_json::Value> {
        self.completed_calls
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// 停止を主張する（Running → Cancelled）。成功したら、以後の `settle_completed`
    /// は DB 永続化も sink 発火も行わない。
    pub fn claim_cancel(&self) -> bool {
        self.state
            .compare_exchange(
                LIFECYCLE_RUNNING,
                LIFECYCLE_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// 決着を主張する（Running → Settling）。成功したら DB 永続化と sink 発火を行う。
    pub fn claim_settle(&self) -> bool {
        self.state
            .compare_exchange(
                LIFECYCLE_RUNNING,
                LIFECYCLE_SETTLING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// すでに停止が確定しているか。
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::SeqCst) == LIFECYCLE_CANCELLED
    }

    /// すでに決着（settle）が確定しているか。
    pub fn is_settling(&self) -> bool {
        self.state.load(Ordering::SeqCst) == LIFECYCLE_SETTLING
    }
}
/// registry が追跡する走行中 subtask のエントリ（gateway 非依存版）。
///
/// 旧 Discord 実装のエントリ型と同型だが、Discord 固有の webhook フィールド
/// （`WebhookConfig` / `DeliveryBatch`）は持たない。
/// 返信ルーティングは gateway 不透明な `reply_target` として spawn 時に捕捉する
/// （RFC §3.1(4)、Nostr で session_id から導出できない問題への対処）。
#[derive(Clone)]
pub struct SpawnedSubtask {
    /// subtask 本体タスクの abort ハンドル（cancel / kill_on_drop 用）。
    pub abort_handle: AbortHandle,
    /// subtask 自身のセッション ID。
    pub session_id: String,
    /// この subtask を spawn した親セッション ID（resume 対象）。
    pub parent_session_id: String,
    /// 実行エージェント ID。
    pub agent_id: String,
    /// 人間可読ラベル（list / cancel での識別用）。
    pub label: String,
    /// **この subtask を生み出したツールの名前**（停止ログの `tool_name` に載る / #184）。
    ///
    /// 明示的な起動なら `spawn_subtask`、非ブロック dispatch で背景化されたツールなら
    /// そのツール名（複数ツールのバッチは `", "` 区切り）。
    /// 以前は停止ログの `tool_name` が常に `spawn_subtask` 固定だったため、
    /// 自動 dispatch されたツールを停止すると会話履歴に
    /// 「spawn_subtask がキャンセルされた / subtask 'execute_shell(...)' was cancelled」
    /// という**矛盾した 2 行**が並んでいた（`crates/server/src/process.rs` の
    /// `tool_cancelled` 整形）。
    pub tool_name: String,
    /// 起動時刻（duration 算出用）。monotonic な `Instant` を用いる。
    pub started_at: std::time::Instant,
    /// gateway 不透明な返信ルーティング token（spawn 時に捕捉）。
    /// settle 時にランタイムが registry から引いて sink へ渡す。
    /// `None` なら返信配送しない。
    pub reply_target: Option<String>,
    /// **この subtask を生んだ親 run の呼び出し元**（#298）。
    ///
    /// 決着時に `settle_completed` / `cancel_subtask` が読み出して
    /// [`SubtaskSettled::caller`] へ載せ、resume する sink が同じ権限で親ターンを
    /// 再開できるようにする。registry はプロセス内メモリで、resume も同一プロセス内
    /// （sink 呼び出し）なので永続化は不要 — プロセスが落ちれば走行中 subtask 自体が
    /// 消え、resume も起きない。
    pub caller: CallerIdentity,
    /// 「停止」と「決着」の排他ラッチ。`cancel_subtask` が `claim_cancel` を、
    /// 走行タスク側の `settle_completed` が `claim_settle` を主張し、先に主張した
    /// 一方だけが有効になる（cancel 後の完了 sink 発火を防ぐ）。
    pub lifecycle: SubtaskLifecycle,
    /// **走行中に追加指示（steer）を受け取れるか**（#647）。
    ///
    /// `true` は明示的な `spawn_subtask`（自前の LLM ループを持ち、反復の合間に
    /// steer ログを読む主体がいる）。`false` は auto-dispatch（ツールを順に実行
    /// するだけで LLM ループが無く、`run_agent_response` を通らないため sub-session
    /// 行も作らない）。後者へ steer しても読む主体がいないので、`steer_subtask` は
    /// `SteerOutcome::NotSteerable` を返して**黙って捨てない**（#647 受け入れ条件 4）。
    ///
    /// 種別で分岐せず能力を明示のフィールドに持つのは、将来の新しい subtask 種別が
    /// この判断を素通りしないようにするため（構築点ごとに steer 可否を宣言させる）。
    pub steerable: bool,
}

/// アクティブな subtask を subtask_id で引く registry（gateway 非依存版）。
///
/// 旧 Discord 実装の registry 型と同型だが gateway 非依存。全ゲートウェイと
/// server がこの型を直接参照する（#170 で Discord 側の re-export は撤去済み）。
pub type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>;

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
