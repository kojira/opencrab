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

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use futures::FutureExt;
use opencrab_core::{
    ActionExecutor, ActionResult, DispatchCall, DispatchOutcome, FunctionDefinition, ToolDispatcher,
};
use tokio::task::AbortHandle;

use crate::traits::CallerIdentity;

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

/// subtask の settle を親セッションへ通知するための最小ペイロード。
///
/// 本文（result）は運搬しない（RFC §1.3）。ここには resume 判断と返信配送に要る
/// 最小情報だけを持つ。
#[derive(Debug, Clone)]
pub struct SubtaskSettled {
    /// 親セッション ID（resume 対象）。
    pub session_id: String,
    /// 親セッションのエージェント ID。
    pub agent_id: String,
    /// settle した subtask の ID。
    pub subtask_id: String,
    /// 決着理由（completed / error / timeout / stopped_by_limit など）。
    /// 種別は `kind` が持つ（progress の二重定義回避）。
    pub exit_reason: String,
    /// 決着の種別（完了 or 進捗）。
    pub kind: SettleKind,
    /// gateway 不透明な返信ルーティング token（#167 / RFC §3.1(4)）。
    ///
    /// ランタイム（`settle_completed`）が registry の `SpawnedSubtask.reply_target`
    /// を引いて載せる。session_id から返信先を復元できない gateway（Nostr の
    /// event id など）のための経路であり、Discord のように session_id から導出
    /// できる sink は `None` のままでよい（無視する）。`None` は「返信配送先の
    /// 指定なし」を意味し、sink 側の既存挙動を変えない。
    pub reply_target: Option<String>,
    /// **この subtask を生んだ親 run の呼び出し元**（#298）。
    ///
    /// resume する sink（Discord / web）はこの値で `RunRequest` を組む。以前は
    /// resume 側が `CallerIdentity::Agent` をハードコードしていたため、オーナー発の
    /// ターンでも subtask が決着した瞬間に権限が降格し、owner/trusted のツールが
    /// list_tools からも dispatch からも丸ごと消えていた（`report_progress` を
    /// 呼ぶと自分の権限が落ちる、という自爆的な挙動）。
    ///
    /// ランタイム（`settle_completed` / `cancel_subtask`）が registry のエントリ
    /// （`SpawnedSubtask.caller`）から読んで載せる。**引き継ぐだけ**で、権限を昇格
    /// させる経路ではない（元が `Agent` のターンは `Agent` のまま）。registry から
    /// 引けなかった場合は最小権限（`Agent`）へ倒す（fail-closed）。
    pub caller: CallerIdentity,
}

/// subtask 完了通知の抽象（`LoopEvent` 直依存を置換する）。
///
/// ランタイムは `Arc<dyn SubtaskCompletionSink>` を保持し、**DB 永続化の後に**
/// `on_subtask_settled` を呼ぶだけで、`LoopEvent` を知らない。sink 実装が
/// 「resume ＋ その gateway の配送口」を担う（Discord=`send_to_channel` /
/// Nostr=`reply` / REST=保存して取得 / heartbeat=次 tick 拾い or 保存）。
pub trait SubtaskCompletionSink: Send + Sync {
    /// 親セッションのエージェントを resume して subtask 結果を会話へ再注入する
    /// トリガ。本文は DB 永続化済みのため運搬しない（RFC §1.3）。
    fn on_subtask_settled(&self, ev: SubtaskSettled);

    /// `cancel_subtask` で subtask が停止したときの通知（`kind = Cancelled`）。
    ///
    /// **完了経路とは別メソッド**にしてある。停止は「resume して返信する」イベント
    /// ではなく、`on_subtask_settled` に流すと resume する sink（Discord / web /
    /// Nostr）が「止めたのに返信する」ことになる。既定実装は debug ログのみで、
    /// 停止で状態整合が必要な sink（REST の `sessions.status`）だけが override する。
    ///
    /// これにより「停止の到達性」が `cancel_subtask` の 1 箇所に閉じる（各経路が
    /// cancel 後に個別に後始末する必要がない）。
    fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
        tracing::debug!(
            session_id = %ev.session_id,
            agent_id = %ev.agent_id,
            subtask_id = %ev.subtask_id,
            exit_reason = %ev.exit_reason,
            "subtask cancelled (sink has no cancel-time reconciliation)"
        );
    }
}

/// 何もしない `SubtaskCompletionSink`（debug ログのみ / #167）。
///
/// `RunRequest::with_dispatch` は sink を必須とするため（`Some(sink)` のときだけ
/// dispatcher を注入する）、「**auto-dispatch だけ有効化して即時の再注入はしない**」
/// 経路にはプレースホルダの sink が必要になる。この sink を渡せば、dispatch した
/// ツールは background 化され、完了本文は `settle_completed` が親セッションログ
/// （DB）へ永続化するだけで終わる。
///
/// 想定用途は heartbeat のように「次 tick で `build_conversation_string` が DB から
/// 完了ログを読み直す」経路（#169）。即時 resume が不要なので、sink 実装を書かずに
/// dispatch を有効化できる。逆に「完了時に即返信したい」gateway ではこれを使わず、
/// 固有 sink を実装する（Discord=`LoopEvent` / Nostr=`reply`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopCompletionSink;

impl SubtaskCompletionSink for NoopCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        tracing::debug!(
            session_id = %ev.session_id,
            agent_id = %ev.agent_id,
            subtask_id = %ev.subtask_id,
            exit_reason = %ev.exit_reason,
            kind = ?ev.kind,
            reply_target = ?ev.reply_target,
            "noop completion sink: subtask settled (no re-injection)"
        );
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
}

/// アクティブな subtask を subtask_id で引く registry（gateway 非依存版）。
///
/// 旧 Discord 実装の registry 型と同型だが gateway 非依存。全ゲートウェイと
/// server がこの型を直接参照する（#170 で Discord 側の re-export は撤去済み）。
pub type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>;

/// `settle_completed` が subtask_completed ログの記録と sink 発火に用いる文脈。
///
/// 本文（result）は別引数で受け取る。DB へは本文込みで永続化するが、sink へ渡す
/// `SubtaskSettled` には本文を載せない（RFC §1.3）。
pub struct SettleContext {
    /// 親セッション ID（resume 対象。空なら DB 記録をスキップ）。
    pub parent_session_id: String,
    /// 実行エージェント ID。
    pub agent_id: String,
    /// settle した subtask の ID。
    pub subtask_id: String,
    /// subtask 自身のセッション ID（ログの session フィールドに載せる）。
    pub sub_session_id: String,
    /// 決着理由（completed / error / timeout / stopped_by_limit）。
    pub exit_reason: String,
    /// 停止/決着の排他ラッチ（`SpawnedSubtask.lifecycle` のクローン）。
    ///
    /// `settle_completed` は **DB 永続化の前に** `claim_settle()` を試み、失敗した
    /// （= `cancel_subtask` が先に停止を主張した）場合は DB 記録も sink 発火も行わない。
    /// registry へ登録しない一発呼び（テスト等）は `SubtaskLifecycle::new()` を渡せば
    /// 常に claim が成功し、従来と同じ挙動になる。
    pub lifecycle: SubtaskLifecycle,
}

/// subtask 完了の中核処理（gateway 非依存 / RFC §4 S1）。
///
/// この関数が **二重回答の順序契約**（RFC §6 受け入れ基準）を 1 箇所で保証する:
///   1. `subtask_completed` を親セッションログ（DB）へ永続化する（本文 `result_text`
///      を含む）。
///   2. registry から当該 subtask を除去し、**その際に取り出したエントリから**
///      `reply_target` を読み出す（#167）。除去してから引き直すことはできない
///      （エントリが消えているため）ので、`remove` の戻り値を使う。これは
///      shard ロック下の 1 操作なので「読んでから消す」間の TOCTOU も無い。
///   3. sink を発火する（本文は運ばない。手順 1 で DB 永続化済み）。`reply_target`
///      は手順 2 で得た値を載せる。
///
/// **DB 永続化（1）は必ず sink 発火（3）より前**に行う。sink 実装（例: Discord）は
/// この後に親セッションを resume し、`build_conversation_string` が DB から会話を
/// 再構築するため、完了ログが先に着地している必要がある。
///
/// gateway 固有の後始末（webhook terminal 送出・progress debounce 除去・随伴構造の
/// 掃除など）は本関数の呼び出し**前**に呼び出し側で行う。それらは DB 永続化とも
/// sink 発火とも順序依存が無い（webhook は非同期配送・別マップ）ため、載せ替えても
/// 観測可能な挙動は変わらない。
pub fn settle_completed(
    registry: &SubtaskRegistry,
    db: &opencrab_db::Db,
    sink: &dyn SubtaskCompletionSink,
    ctx: SettleContext,
    result_text: &str,
) {
    // 0. 停止と決着の排他（DB 永続化より前）。`cancel_subtask` が先に停止を主張して
    //    いたら、ここでは **DB 記録も registry 除去も sink 発火もしない**。
    //    ツール完走〜DB INSERT の窓で cancel が入ったとき、`cancelled:true` を返した
    //    のに完了ログが着地して sink が resume する（＝止めたのに返信が届く）のを防ぐ。
    if !ctx.lifecycle.claim_settle() {
        tracing::debug!(
            session_id = %ctx.parent_session_id,
            subtask_id = %ctx.subtask_id,
            exit_reason = %ctx.exit_reason,
            "subtask was cancelled before settling; skipping persistence and sink"
        );
        return;
    }

    // 1. 完了本文を DB へ永続化する（sink 発火より前 = 順序契約）。
    if !ctx.parent_session_id.is_empty() {
        if let Ok(conn) = db.lock() {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: ctx.agent_id.clone(),
                session_id: ctx.parent_session_id.clone(),
                log_type: "system".to_string(),
                content: serde_json::json!({
                    "type": "subtask_completed",
                    "subtask_id": ctx.subtask_id,
                    "session_id": ctx.sub_session_id,
                    "exit_reason": ctx.exit_reason,
                    "result": result_text,
                })
                .to_string(),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
        }
    }

    // 2. registry から除去し、除去したエントリから reply_target と caller を回収する。
    //    remove 後は引けないため、remove の戻り値から読み出す（#167 / #298）。
    let removed = registry.remove(&ctx.subtask_id).map(|(_, subtask)| subtask);
    let reply_target = removed.as_ref().and_then(|s| s.reply_target.clone());
    // registry に載っていない一発呼び（テスト等）は最小権限へ倒す（fail-closed）。
    let caller = removed
        .map(|s| s.caller)
        .unwrap_or(crate::traits::CallerIdentity::Agent);

    // 3. sink を発火する（本文は運ばない = DB 永続化済み）。
    sink.on_subtask_settled(SubtaskSettled {
        session_id: ctx.parent_session_id,
        agent_id: ctx.agent_id,
        subtask_id: ctx.subtask_id,
        exit_reason: ctx.exit_reason,
        kind: SettleKind::Completed,
        reply_target,
        caller,
    });
}

/// `cancel_subtask` の結果種別（gateway 非依存 / #161）。
///
/// gateway 別の戻り値整形（`GatewayActionResult` の success/error）は呼び出し側が
/// 行う。ここでは「停止した / 不在 / 権限なし」の三値だけを型で返し、認可と registry
/// 操作を 1 箇所（`cancel_subtask`）に集約する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 対象を abort し registry から除去した。
    Cancelled,
    /// 対象 `subtask_id` が registry に存在しない。
    NotFound,
    /// 存在するが呼び出し元に権限が無い（親セッション/owner 以外）。
    Unauthorized,
}

/// 走行中 subtask を停止する中核処理（gateway 非依存 / #161・#157 S2）。
///
/// web / Nostr / REST など Discord 以外の transport でも `cancel_subtask` ツールを
/// 露出できるよう、認可・abort・registry 除去・親ログ記録・lifecycle 通知を
/// server-neutral 層へ集約する。**停止の実装はこの 1 関数だけ**で、transport 固有の
/// 実装は持たない（#157 S2 で Discord 実装を撤去し、その固有の後始末をここへ取り込んだ）。
///
/// # Discord 実装から取り込んだ 2 点（#157 S2）
///
/// 1. **中断の通知送出**: `notifiers`（registry と対の随伴マップ）から通知口を引いて
///    `on_cancelled(duration_ms)` を呼び、マップから外す。abort すると spawned closure は
///    中断されて終了通知が来ないため、ここが lifecycle 通知の唯一の終端になる（RFC §1.5）。
///    **順序契約との関係**: この通知は親ログ INSERT より前だが、実装（Discord の
///    `SubtaskWebhookNotifier::on_cancelled`）は webhook 配送キューへ 1 通積むだけで
///    応答生成を起動しない。「記録 → registry 除去 → sink 発火」という二重返信防止の
///    順序契約に触れるのは resume する `sink` 側だけで、そちらは従来どおり INSERT の後。
/// 2. **停止ログの説明文の解決順序**: sub-session の `sessions.theme`（`Subtask: ` prefix を
///    除去）を第一候補にし、引けない/空のときだけ registry の `label`
///    （例: `execute_shell(...)`）へフォールバックする。明示的な `spawn_subtask` は
///    人間可読なテーマを持つが、自動 dispatch は sub-session の行を作らないため theme を
///    引けず、そのままだと親ログが `subtask '' was cancelled` になる（#176）。
///
/// 認可（#64）: `is_owner` なら常に許可。そうでなければ「呼び出し元セッションが親
/// （`parent_session_id == caller_session_id`）」の subtask のみ停止できる（自己/兄弟/
/// 他セッションのものは不可）。`remove_if` は shard ロック下で述語を評価するため、
/// 「認可確認 → 削除」の間にエントリが差し替わる TOCTOU が無い（所有権フィールドは
/// insert 後不変）。
///
/// 成功時: **停止を主張（`claim_cancel`）** → `abort_handle.abort()` → registry から
/// 除去 → 通知口へ `on_cancelled` → 親セッションログへ `tool_cancelled` を best-effort
/// 記録 → sink へ `on_subtask_cancelled`（`exit_reason="cancelled"`）を通知する。
/// この順序は旧 Discord 実装（通知が親ログより先）と neutral 実装（親ログが sink より
/// 先）の両方を満たす。
///
/// 停止の主張は registry 除去と同じ shard ロック下（`remove_if` の述語内）で行う。
/// `abort()` は「ツール本体を await 中」なら効くが、既に完走して `settle_completed`
/// へ入っている場合は効かない。そこでラッチで排他し、cancel が勝ったときは
/// `settle_completed` 側が DB 記録も sink 発火も諦める（＝完了イベント無し）。
/// 逆に settle が先に主張していた場合は停止できないので `NotFound` を返す
/// （その subtask は通常完了として通知される）。
///
/// `sink` を渡すと停止も 1 箇所から通知でき、経路側（REST の `sessions.status` 等）が
/// cancel 後に個別に整合を取る必要がなくなる。既定実装は debug ログのみなので、
/// resume する sink（Discord / web / Nostr）の挙動は変わらない。
#[allow(clippy::too_many_arguments)]
pub fn cancel_subtask(
    registry: &SubtaskRegistry,
    db: &opencrab_db::Db,
    sink: Option<&dyn SubtaskCompletionSink>,
    notifiers: Option<&crate::subtask_notify::SubtaskNotifiers>,
    subtask_id: &str,
    is_owner: bool,
    caller_session_id: Option<&str>,
) -> CancelOutcome {
    let authorized = |s: &SpawnedSubtask| -> bool {
        if is_owner {
            return true;
        }
        matches!(caller_session_id, Some(cs) if !cs.is_empty() && s.parent_session_id == cs)
    };

    // 述語は shard ロック下で評価される。認可 → 停止の主張（CAS）→ 除去を 1 操作に
    // まとめるため、認可も claim も述語内で行う（claim に失敗＝決着済みなら除去しない）。
    match registry.remove_if(subtask_id, |_, s| {
        authorized(s) && s.lifecycle.claim_cancel()
    }) {
        Some((_, subtask)) => {
            subtask.abort_handle.abort();

            // 中断を lifecycle 通知口へ伝え、随伴マップから外す（旧 Discord 実装から移設
            // / RFC §1.5）。abort で spawned closure は中断されるため終了通知は来ない
            // → ここが唯一の終端。親ログ INSERT より**前**に呼ぶ（旧実装と同順序）。
            if let Some(notifiers) = notifiers {
                if let Some((_, notifier)) = notifiers.remove(subtask_id) {
                    notifier.on_cancelled(subtask.started_at.elapsed().as_millis() as u64);
                }
            }

            // 親セッションログへ subtask_cancelled を best-effort 記録する。
            //
            // **部分結果も残す**（#152 レビュー P2）。1 バッチ = 1 subtask なので、
            // 停止時に `settle_completed` を丸ごと抑止するとラベルしか残らず「3 ファイル
            // 書いた後に止めた」ときにどこまで進んだか分からない。完走済み call を本文へ
            // 列挙し（人が読む/会話へ再注入される）、構造は metadata に載せる。
            let parent = subtask.parent_session_id.clone();
            if !parent.is_empty() {
                let completed = subtask.lifecycle.completed_calls();
                if let Ok(conn) = db.lock() {
                    // 停止対象の説明は sub-session の theme を第一候補にする（旧 Discord
                    // 実装から移設 / #176）。明示的な `spawn_subtask` はここに人間可読な
                    // テーマを持つが、自動 dispatch は sub-session の行を作らないため
                    // theme を引けない。引けない/空のときは registry の label
                    // （例: `execute_shell(...)`）へフォールバックする。
                    let task_description =
                        opencrab_db::queries::get_session(&conn, &subtask.session_id)
                            .ok()
                            .flatten()
                            .map(|session| {
                                session
                                    .theme
                                    .strip_prefix("Subtask: ")
                                    .unwrap_or(&session.theme)
                                    .to_string()
                            })
                            .filter(|desc| !desc.is_empty())
                            .unwrap_or_else(|| subtask.label.clone());
                    let content = if completed.is_empty() {
                        format!("subtask '{task_description}' was cancelled")
                    } else {
                        let partial =
                            serde_json::to_string(&completed).unwrap_or_else(|_| "[]".to_string());
                        format!(
                            "subtask '{}' was cancelled after {} completed tool call(s): {partial}",
                            task_description,
                            completed.len()
                        )
                    };
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: subtask.agent_id.clone(),
                        session_id: parent.clone(),
                        log_type: "tool_cancelled".to_string(),
                        content,
                        speaker_id: None,
                        turn_number: None,
                        metadata_json: Some(
                            // `task` は旧 Discord 実装のキー、`label` / `completed_calls`
                            // は neutral 実装のキー。統合後は**両方**載せる（どちらの
                            // 読み手も壊さない）。`tool_name` は固定値ではなく
                            // **実際に停止したツール名**（#184）。
                            serde_json::json!({
                                "tool_call_id": subtask_id,
                                "tool_name": subtask.tool_name,
                                "task": task_description,
                                "label": subtask.label,
                                "completed_calls": completed,
                            })
                            .to_string(),
                        ),
                        created_at: None,
                    };
                    opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
                }
            }

            // 停止を sink へ通知する（完了経路とは別メソッド = resume しない）。
            // これで「最後の subtask が cancel されたのに誰もセッションを完了に
            // しない」（REST が永久 active）が起きない。
            if let Some(sink) = sink {
                sink.on_subtask_cancelled(SubtaskSettled {
                    session_id: parent,
                    agent_id: subtask.agent_id.clone(),
                    subtask_id: subtask_id.to_string(),
                    exit_reason: "cancelled".to_string(),
                    kind: SettleKind::Cancelled,
                    reply_target: subtask.reply_target.clone(),
                    caller: subtask.caller.clone(),
                });
            }
            CancelOutcome::Cancelled
        }
        None => {
            // remove_if の None は「不在」「権限なし」「既に決着（settle）済み」。
            // 所有権フィールドは insert 後不変なので contains_key で不在と区別でき、
            // 残っていて claim に失敗した場合（決着済み = もう停止できない）は
            // NotFound として扱う（停止対象として存在しない）。
            match registry.get(subtask_id) {
                Some(entry) if !entry.lifecycle.is_settling() => CancelOutcome::Unauthorized,
                Some(_) => CancelOutcome::NotFound,
                None => CancelOutcome::NotFound,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// S3a: ツール呼び出しバッチを subtask として実行する dispatcher（非ブロック / 全ツール自動化）
// ---------------------------------------------------------------------------

/// `Arc<dyn ActionExecutor>` を `ActionExecutor` として使えるようにする薄い委譲ラッパ。
///
/// `SkillEngine::new` は `Box<dyn ActionExecutor>` を取るが、S3a では同じ合成 executor
/// を engine（inline 実行用）と `SubtaskToolDispatcher`（background 実行用）で**共有**
/// したい。executor を 1 つの `Arc` にまとめ、engine には `Box::new(SharedExecutor(arc))`、
/// dispatcher には `arc.clone()` を渡すことで、`nostr_generate_key` 等 server ツールを
/// 含む合成 gateway 到達性（RFC #152 S2）を dispatch 経路でも共有する。
pub struct SharedExecutor(pub Arc<dyn ActionExecutor>);

#[async_trait::async_trait]
impl ActionExecutor for SharedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> ActionResult {
        self.0.execute(name, args).await
    }
    async fn execute_with_id(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: &str,
    ) -> ActionResult {
        self.0.execute_with_id(name, args, tool_call_id).await
    }
    fn list_tools(&self) -> Vec<FunctionDefinition> {
        self.0.list_tools()
    }
}

/// 既定で auto-dispatch **しない**（＝ inline 実行のまま）ツール名の集合。
///
/// # 分類基準（この 6 つのどれかに当てはまるツールは inline に残す）
///
/// 1. **制御系**（`spawn_subtask` / `cancel_subtask` / `report_progress`）: それ自体が
///    subtask ライフサイクルを操作するため background 化しない。
/// 2. **配送系**（Discord 送信・A2UI 送信・VC 参加/退出・peer review 依頼・Nostr 送信）:
///    「送る」こと自体が応答であり、background 化して完了で再注入する意味がない。加えて
///    gateway が「明示送信したか」を親ターンの終わりに見て暗黙返信を抑制する場合
///    （Nostr）、background 化は**二重投稿**を生む。`send_ui` はさらに**ユーザーの応答を
///    待機する** pending interaction を張るため、background 化すると (a) UI 投稿と本文
///    返信の順序が入れ替わり、(b) エージェントは `spawned` しか見えずインタラクション
///    ID を扱えず、(c) クリックによる resume と subtask 決着の resume で返信が 2 通になる。
/// 3. **同ターン結果依存**: 戻り値（URL / ID）をそのターンの後続処理や応答本文に使う用法が
///    通常のもの（`ensure_webhook` / `ensure_subtask_webhook` / `discord_create_webhook` /
///    `discord_create_channel` / `nostr_upload`）。background 化すると値の代わりに
///    `spawned` が返るだけなので、用法そのものが壊れる。
/// 4. **run 内共有状態を書くツール**（`select_llm` / `discord_channel_config` /
///    `nostr_switch_identity`）: `ActionContext` の `model_override` / `current_purpose`、
///    チャンネルの writable、以後の送信 identity のように、走行中の run（や同ターンの配送）
///    が参照している状態を書き換える。background 化すると (a) そのターンには効かず、
///    (b) 書き込みが engine の次イテレーションや配送と競合して「いつのターンから効くか」が
///    非決定になる。制御の効き方を保つため inline に残す。
/// 5. **純粋な読み取りで即答すべきもの**（`list_*` / `get_*` / `ws_read` / `ws_list` /
///    `search_memory_index` / `retrieve_memory_nodes` / `read_heartbeat_instructions`）:
///    dispatch すると質問 1 つが 2 ターン 2 メッセージに割れるだけで、得るものが無い。
///    system prompt が指示する記憶想起フロー（`search_memory_index` →
///    `retrieve_memory_nodes`）のような**同ターンの 2 段連鎖**では、背景往復が 2 回 =
///    ユーザーへ 4 通になる。
/// 6. **報告する価値が無い短時間の書き込み**（`update_impression` / `save_model_insight` /
///    `record_task_progress`）: dispatch には必ず resume ターン（= ユーザーへの追加
///    メッセージ）が 1 本付く。値の小さい書き込みを background 化すると雑音が増えるだけ。
///
/// 逆に dispatch を**残す**のは「長時間かかる」か「同ターンで結果を使わない書き込み」のみ
/// （`rebuild_memory_index` / `create_skill` / `nostr_generate_key` / `ws_write` /
/// `learn_from_experience` / server ツールの `execute_shell` …）。
///
/// # MCP ツール（`mcp__*`）
///
/// **既定 inline**（安全側）。運用者が繋いだ任意の外部ツールなので、配送系（外部へ
/// 「送る」）なのか同ターンで戻り値を使うのかを静的に判定できない。無分類で全 dispatch
/// すると、外部送信系 MCP が background 化されて配送順が入れ替わる/二重送信になる。
/// 判定は [`SubtaskToolDispatcher::should_dispatch`] が名前の接頭辞で行う（一覧に列挙
/// できないため集合ではなく規則で扱う）。長時間の MCP ツールを background 化したい
/// 運用者は `with_non_dispatch` で当該名を除いた集合を渡す。
///
/// # ドリフト検出
///
/// core（[`crate::dispatcher::ActionDispatcher`]）と全 gateway（Discord / Nostr /
/// server 内蔵の `SystemGatewayActions`）の `definitions()` の**全名**が「この集合に
/// ある」か「明示的な dispatch 可リスト
/// （[`crate::bridge::CORE_DISPATCHABLE_ACTIONS`] /
/// [`crate::bridge::DISCORD_DISPATCHABLE_ACTIONS`] /
/// [`crate::bridge::NOSTR_DISPATCHABLE_ACTIONS`] /
/// [`crate::bridge::SERVER_DISPATCHABLE_ACTIONS`]）にある」かのどちらかであることを、
/// fail-closed テストが検査する（core は `core_actions_are_classified_for_dispatch`、
/// gateway は各 gateway 実装の crate 側 = `crates/discord` / `crates/nostr` /
/// `crates/server`）。新ツールを追加すると、どちらにも入れない限りテストが落ちる
/// （= 分類を強制する）。
///
/// 呼び出し側は `SubtaskToolDispatcher::with_non_dispatch` で上書き/追加できる。
/// 運用者向けの**分類基準**は `docs/DESIGN.md`「非ブロックツール実行」節。
/// ツール名の一覧はこの定数群が唯一の権威で、doc 側には置かない（二重管理を避ける）。
pub fn default_non_dispatch_tools() -> HashSet<String> {
    let mut set: HashSet<String> = ["spawn_subtask", "cancel_subtask", "report_progress"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // core アクション（`ActionDispatcher::new()`）の inline 集合。以前はここが空で、
    // core 32 個が分類ガードの外＝全 dispatch だった（記憶想起が 4 通に割れる等）。
    for name in crate::bridge::CORE_INLINE_ACTIONS {
        set.insert((*name).to_string());
    }
    // Discord gateway の inline 集合（配送系 / 同ターン結果依存 / run 内共有状態 /
    // 純粋な読み取り）。depth ゲートの `DISCORD_ACTIONS` とは目的が違う別集合で、
    // `DISCORD_ACTIONS ⊆ DISCORD_INLINE_ACTIONS` はテストで保証する。
    for name in crate::bridge::DISCORD_INLINE_ACTIONS {
        set.insert((*name).to_string());
    }
    // Nostr 配送系（#168）。background 化すると親ターンが「明示送信済み」フラグを
    // 観測できず、暗黙返信と二重投稿になる。`nostr_generate_key` は含まない
    // （長時間処理なので dispatch 対象に残す）。
    for name in crate::bridge::NOSTR_DELIVERY_ACTIONS {
        set.insert((*name).to_string());
    }
    // server 内蔵の設定ツール源（`SystemGatewayActions`）の inline 集合。transport
    // 非依存で web / REST / heartbeat の全ターンに載るのに分類ガードの外にあり、
    // 設定変更（run 内共有状態）と一覧（純粋な読み取り）が background 化されていた。
    // `nostr_generate_key`（長時間の鍵探索）だけは dispatch 対象に残す。
    for name in crate::bridge::SERVER_INLINE_ACTIONS {
        set.insert((*name).to_string());
    }
    set
}

/// 「同一バッチのツール呼び出しを background subtask として実行する」job のランタイム
/// （RFC #152 S3a）。
///
/// `execute_spawn_subtask` が sub-engine（LLM ループ）を建てるのに対し、これは
/// **指定ツールを合成 executor で順に実行するだけ**の job。spawn / registry 登録
/// （`SpawnedSubtask`, label=`tool(主要引数)`）/ 完了時 `settle_completed`（DB 永続化
/// → registry 除去 → sink 発火）という中核は既存と共有し、job の中身だけ差し替える。
///
/// 不変条件:
/// - **1 バッチ = 1 subtask**。同一 assistant メッセージのツールは並行実行せず
///   `calls` の順に逐次実行する（`write_file` → `execute_shell` の依存順を守る）。
///   結果として完了通知（親の resume）も 1 バッチにつき 1 回で済む。
/// - **必ず決着する**。ツールがハングしても既定タイムアウト
///   （`DEFAULT_DISPATCH_TIMEOUT_SECS`）で `exit_reason="timeout"`、panic しても
///   `catch_unwind` で `exit_reason="error"` として settle し、registry に死骸を残さない。
/// - **cancel と競合しない**。`SubtaskLifecycle` により停止と決着は排他。
///
/// 既知の残課題: 1 run の**別イテレーション**でそれぞれツールが dispatch された場合
/// （LLM が spawned マーカーを見た後にさらにツールを呼ぶ）は subtask が 2 本になり、
/// 完了 sink も 2 回発火する。バッチ内（同一 assistant メッセージ）の N ツールは 1 本に
/// まとまるため、レビューで実証された「1 ターン N ツール → N 通の返信」は解消する。
///
/// `core::ToolDispatcher` を実装し、`SkillEngine` のツール実行点から呼ばれる。
pub struct SubtaskToolDispatcher {
    /// dispatch したツールを実行する合成 executor（server ツールを含む）。
    executor: Arc<dyn ActionExecutor>,
    /// 走行中 subtask の registry（settle 時に除去 / 将来の cancel/list 用）。
    registry: SubtaskRegistry,
    db: opencrab_db::Db,
    /// 完了再注入 sink（gateway 別。Discord=LoopEvent / Nostr=reply ...）。
    sink: Arc<dyn SubtaskCompletionSink>,
    agent_id: String,
    parent_session_id: String,
    /// auto-dispatch しないツール名（既定 = `default_non_dispatch_tools()`）。
    non_dispatch: HashSet<String>,
    /// dispatch した subtask に付与する gateway 不透明な返信ルーティング token
    /// （#167）。この dispatcher は 1 回の親ターンに紐づくため、inbound の返信先を
    /// そのまま全 dispatch へ引き継ぐ。`None` なら返信配送先の指定なし（Discord は
    /// session_id から復元するため `None` のままでよい）。
    reply_target: Option<String>,
    /// 親 run（この dispatcher を持つターン）の呼び出し元（#298）。
    ///
    /// dispatch した `SpawnedSubtask.caller` に載り、settle 時に
    /// `SubtaskSettled.caller` として sink へ届く。resume が元の権限を落とさない
    /// ための引き継ぎで、既定は最小権限（`Agent`）。
    caller: CallerIdentity,
    /// バッチ全体の実行時間上限。超過すると `exit_reason="timeout"` で settle する。
    timeout: std::time::Duration,
    /// 上限超過の tool_result を退避するエージェントのワークスペース root
    /// （inline 経路 `process.rs` の `tool_result_workspace` と同じもの）。
    /// `None` なら退避せず切り詰める。
    workspace_root: Option<std::path::PathBuf>,
}

impl SubtaskToolDispatcher {
    pub fn new(
        executor: Arc<dyn ActionExecutor>,
        registry: SubtaskRegistry,
        db: opencrab_db::Db,
        sink: Arc<dyn SubtaskCompletionSink>,
        agent_id: impl Into<String>,
        parent_session_id: impl Into<String>,
    ) -> Self {
        Self {
            executor,
            registry,
            db,
            sink,
            agent_id: agent_id.into(),
            parent_session_id: parent_session_id.into(),
            non_dispatch: default_non_dispatch_tools(),
            reply_target: None,
            caller: CallerIdentity::Agent,
            timeout: std::time::Duration::from_secs(DEFAULT_DISPATCH_TIMEOUT_SECS),
            workspace_root: None,
        }
    }

    /// auto-dispatch 対象外の集合を差し替える。
    pub fn with_non_dispatch(mut self, non_dispatch: HashSet<String>) -> Self {
        self.non_dispatch = non_dispatch;
        self
    }

    /// バッチ実行のタイムアウトを差し替える（既定 = `DEFAULT_DISPATCH_TIMEOUT_SECS`）。
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 大きい tool_result の退避先（エージェントのワークスペース root）を設定する。
    ///
    /// inline 経路と同じ扱い（上限超過はファイルへ退避してポインタだけ DB に残す）を
    /// dispatch 経路でも行うために必要。未設定なら切り詰めのみ。
    pub fn with_workspace_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.workspace_root = root;
        self
    }

    /// dispatch する subtask に載せる返信ルーティング token を設定する（#167）。
    ///
    /// settle 時に `settle_completed` が registry から引いて `SubtaskSettled`
    /// へ載せるため、sink は session_id に依らず返信先を得られる。
    pub fn with_reply_target(mut self, reply_target: Option<String>) -> Self {
        self.reply_target = reply_target;
        self
    }

    /// dispatch する subtask に載せる**親 run の呼び出し元**を設定する（#298）。
    ///
    /// settle 時に `settle_completed` が registry から引いて `SubtaskSettled` へ載せる
    /// ため、resume する sink は元のターンと同じ権限で親を再開できる。未設定なら
    /// 最小権限（`Agent`）のまま = 従来挙動。
    pub fn with_caller(mut self, caller: CallerIdentity) -> Self {
        self.caller = caller;
        self
    }
}

/// `tool(主要引数)` 形式のラベルを組む。args が object なら最初のキーの値（scalar）を
/// 40 文字までプレビューする。
fn dispatch_label(tool_name: &str, args: &serde_json::Value) -> String {
    let preview = args
        .as_object()
        .and_then(|m| m.values().next())
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let preview: String = preview.chars().take(40).collect();
    if preview.is_empty() {
        format!("{tool_name}()")
    } else {
        format!("{tool_name}({preview})")
    }
}

/// バッチが背景化したツール名（重複を除いた `", "` 区切り / #184）。
///
/// 停止ログの `tool_name` に載る。以前は `spawn_subtask` 固定だったため、自動 dispatch
/// されたツールを停止すると「spawn_subtask がキャンセルされた」と描画され、直後の
/// 本文行（`subtask 'execute_shell(...)' was cancelled`）と矛盾していた。
fn batch_tool_names(calls: &[DispatchCall]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for c in calls {
        if !names.contains(&c.tool_name.as_str()) {
            names.push(&c.tool_name);
        }
    }
    names.join(", ")
}

/// バッチ全体のラベル（`tool(arg), tool(arg)` を 120 文字で打ち切る）。
fn batch_label(calls: &[DispatchCall]) -> String {
    let joined = calls
        .iter()
        .map(|c| dispatch_label(&c.tool_name, &c.args))
        .collect::<Vec<_>>()
        .join(", ");
    let truncated: String = joined.chars().take(120).collect();
    if truncated.len() < joined.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// 1 ツールの実行結果（バッチの決着理由と本文の組み立てに使う）。
struct CallOutcome {
    /// 呼び出し元 LLM の tool_call_id（同じツールを複数回呼んだバッチの対応付け）。
    tool_call_id: String,
    /// 永続化用に無害化済みの結果本文。
    sanitized: String,
    /// このツールが失敗（`success == false` / panic / 未実行）したか。
    failed: bool,
}

/// 複数ツールバッチの完了本文 1 要素を組む。
///
/// `sanitized` は原則 JSON 文字列なので、**文字列としてではなく構造として**埋める
/// （`serde_json::from_str` を試す）。文字列のまま入れると
/// `"result":"[{\"result\":\"{\\\"success\\\":true…"` のような三重エスケープになり、
/// 会話へ再注入するときの整形でさらにエスケープが増えてモデルが読めなくなる。
/// 上限超過時のメタ情報案内（`[Tool result withheld: … bytes, … lines …]`）のように
/// JSON にならない場合だけ文字列で入れる。
///
/// `tool_call_id` も必ず載せる（同じツールを複数回呼んだバッチは順序でしか対応が
/// 取れなかった）。
fn batch_result_entry(tool: &str, outcome: &CallOutcome) -> serde_json::Value {
    let result = serde_json::from_str::<serde_json::Value>(&outcome.sanitized)
        .unwrap_or_else(|_| serde_json::Value::String(outcome.sanitized.clone()));
    serde_json::json!({
        "tool": tool,
        "tool_call_id": outcome.tool_call_id,
        "result": result,
    })
}

impl ToolDispatcher for SubtaskToolDispatcher {
    fn should_dispatch(&self, tool_name: &str) -> bool {
        // MCP ツール（`mcp__*`）は既定 inline（安全側）。運用者が繋いだ任意ツールなので
        // 配送系かどうかを静的に分類できず、全 dispatch すると外部送信系 MCP が
        // background 化されて配送順の入れ替わり/二重送信になる。名前を静的に列挙できない
        // ため集合ではなく接頭辞の規則で扱う（`default_non_dispatch_tools` の doc 参照）。
        if tool_name.starts_with(crate::bridge::MCP_TOOL_PREFIX) {
            return false;
        }
        !self.non_dispatch.contains(tool_name)
    }

    fn dispatch_batch(&self, calls: &[DispatchCall]) -> DispatchOutcome {
        let subtask_id = uuid::Uuid::new_v4().to_string();
        let sub_session_id = format!("subtask-{subtask_id}");
        let label = batch_label(calls);
        // 停止ログに載せる「実際に背景化したツール名」（#184）。
        let tool_names = batch_tool_names(calls);

        // 各種クローン（background タスクへムーブ）。
        let executor = self.executor.clone();
        let registry = self.registry.clone();
        let db = self.db.clone();
        let sink = self.sink.clone();
        let agent_id = self.agent_id.clone();
        let parent_session_id = self.parent_session_id.clone();
        let calls_owned = calls.to_vec();
        let subtask_id_task = subtask_id.clone();
        let sub_session_id_task = sub_session_id.clone();
        let timeout = self.timeout;
        let workspace_root = self.workspace_root.clone();
        // 停止/決着の排他ラッチ。registry のエントリ（cancel 側）とタスク本体
        // （settle 側）が同じラッチを共有する。
        let lifecycle = SubtaskLifecycle::new();
        let lifecycle_task = lifecycle.clone();

        // 開始ゲート: 親が registry へ insert し終えるまでタスク本体を走らせない
        // （即完了する job が親の insert より先に remove して running のままリークするのを防ぐ。
        //  execute_spawn_subtask と同じ不変条件）。
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            let _ = start_rx.await;

            // バッチ全体に 1 つの期限を与える（ハングしたツールで永久滞留しない）。
            let deadline = tokio::time::Instant::now() + timeout;
            let mut outcomes: Vec<(String, CallOutcome)> = Vec::with_capacity(calls_owned.len());
            let mut timed_out = false;

            // **逐次実行**（並行にしない）: LLM が並べた順序に依存関係があり得る。
            for call in &calls_owned {
                // panic を settle へ変換する（catch しないと task が unwind して
                // settle を通らず、registry に死骸が残り REST は永久 active になる）。
                let fut = std::panic::AssertUnwindSafe(executor.execute_with_id(
                    &call.tool_name,
                    &call.args,
                    &call.tool_call_id,
                ))
                .catch_unwind();

                let raw = match tokio::time::timeout_at(deadline, fut).await {
                    Ok(Ok(result)) => {
                        let failed = !result.success;
                        let json = serde_json::to_string(&result)
                            .unwrap_or_else(|_| r#"{"success":false}"#.to_string());
                        (json, failed)
                    }
                    Ok(Err(panic_payload)) => {
                        let msg = panic_message(&panic_payload);
                        tracing::error!(
                            tool = %call.tool_name,
                            subtask_id = %subtask_id_task,
                            "dispatched tool panicked; settling as error"
                        );
                        (
                            serde_json::json!({
                                "success": false,
                                "data": null,
                                "error": format!("tool '{}' panicked: {msg}", call.tool_name),
                            })
                            .to_string(),
                            true,
                        )
                    }
                    Err(_elapsed) => {
                        timed_out = true;
                        (
                            serde_json::json!({
                                "success": false,
                                "data": null,
                                "error": format!(
                                    "tool '{}' timed out after {}s",
                                    call.tool_name,
                                    timeout.as_secs()
                                ),
                            })
                            .to_string(),
                            true,
                        )
                    }
                };

                // inline 経路（process.rs の on_tool_result）と**同一**の無害化を通す:
                // 秘密フィールドのマスク ＋ サイズ上限/ワークスペース退避。
                let sanitized = opencrab_core::tool_result_log::sanitize_tool_result_for_log(
                    &call.tool_name,
                    &raw.0,
                    &parent_session_id,
                    &call.tool_call_id,
                    workspace_root.as_deref(),
                );
                let outcome = CallOutcome {
                    tool_call_id: call.tool_call_id.clone(),
                    sanitized,
                    failed: raw.1,
                };
                // cancel されたときに「どこまで進んだか」を親へ残せるよう、完走した
                // call の部分結果をラッチ（cancel 側と共有）へ積む。
                lifecycle_task.record_completed_call(batch_result_entry(&call.tool_name, &outcome));
                outcomes.push((call.tool_name.clone(), outcome));
                if timed_out {
                    // 期限切れ後は残りを実行しない（依存順のため後続は前提が崩れている）。
                    //
                    // ただし**未実行であることは本文に残す**（#152 レビュー P2）。
                    // system prompt は「同じツールを再呼びするな（もう走っている）」と
                    // 指示しているので、痕跡が無いとエージェントは未実行を知る手段が無く
                    // 依頼が無言で消える。
                    for skipped in calls_owned.iter().skip(outcomes.len()) {
                        outcomes.push((
                            skipped.tool_name.clone(),
                            CallOutcome {
                                tool_call_id: skipped.tool_call_id.clone(),
                                sanitized: serde_json::json!({
                                    "success": false,
                                    "data": null,
                                    "error": "skipped: batch timed out",
                                })
                                .to_string(),
                                failed: true,
                            },
                        ));
                    }
                    break;
                }
            }

            let exit_reason = if timed_out {
                "timeout"
            } else if outcomes.iter().any(|(_, o)| o.failed) {
                "error"
            } else {
                "completed"
            };
            // 完了本文（DB へ永続化。sink には運ばない = RFC §1.3）。
            // 単一ツールのときは従来と同じ「ツール結果 JSON そのまま」を保つ。
            let result_text = if calls_owned.len() == 1 {
                outcomes
                    .first()
                    .map(|(_, o)| o.sanitized.clone())
                    .unwrap_or_else(|| r#"{"success":false}"#.to_string())
            } else {
                // 複数ツール: 結果を**構造として**埋め、`tool_call_id` も載せる
                // （三重エスケープと対応付け不能の解消 = レビュー P2）。
                let arr: Vec<serde_json::Value> = outcomes
                    .iter()
                    .map(|(tool, o)| batch_result_entry(tool, o))
                    .collect();
                serde_json::to_string(&arr).unwrap_or_else(|_| r#"[]"#.to_string())
            };

            // 中核（gateway 非依存）: DB 永続化 → registry 除去 → sink 発火。
            // 順序契約（DB 記録 → 通知）は settle_completed が 1 箇所で保証する。
            settle_completed(
                &registry,
                &db,
                sink.as_ref(),
                SettleContext {
                    parent_session_id,
                    agent_id,
                    subtask_id: subtask_id_task,
                    sub_session_id: sub_session_id_task,
                    exit_reason: exit_reason.to_string(),
                    lifecycle: lifecycle_task,
                },
                &result_text,
            );
        });

        // registry へ登録（開始ゲート解放の前 = insert-before-run）。
        self.registry.insert(
            subtask_id.clone(),
            SpawnedSubtask {
                abort_handle: join.abort_handle(),
                session_id: sub_session_id,
                parent_session_id: self.parent_session_id.clone(),
                agent_id: self.agent_id.clone(),
                label: label.clone(),
                tool_name: tool_names,
                started_at: std::time::Instant::now(),
                reply_target: self.reply_target.clone(),
                // 親ターンの呼び出し元をそのまま引き継ぐ（#298）。
                caller: self.caller.clone(),
                lifecycle,
            },
        );
        // insert 完了 → タスク本体の実行を許可する。
        let _ = start_tx.send(());

        DispatchOutcome { subtask_id, label }
    }
}

/// `catch_unwind` の payload から人間可読なメッセージを取り出す。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `SubtaskCompletionSink` の最小フェイク実装。受け取った settle を記録する。
    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<SubtaskSettled>>,
        /// 停止通知（`on_subtask_cancelled`）を別に記録する。
        cancelled: Mutex<Vec<SubtaskSettled>>,
    }

    impl SubtaskCompletionSink for RecordingSink {
        fn on_subtask_settled(&self, ev: SubtaskSettled) {
            self.events.lock().unwrap().push(ev);
        }
        fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
            self.cancelled.lock().unwrap().push(ev);
        }
    }

    /// 単一ツールを 1 バッチとして dispatch するテストヘルパ
    /// （engine は `dispatch_batch` しか呼ばない）。
    fn dispatch_one(
        dispatcher: &SubtaskToolDispatcher,
        tool_name: &str,
        args: serde_json::Value,
        tool_call_id: &str,
    ) -> DispatchOutcome {
        dispatcher.dispatch_batch(&[DispatchCall {
            tool_name: tool_name.to_string(),
            args,
            tool_call_id: tool_call_id.to_string(),
        }])
    }

    #[test]
    fn sink_receives_settled_event() {
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(RecordingSink::default());
        sink.on_subtask_settled(SubtaskSettled {
            session_id: "discord-123".to_string(),
            agent_id: "agent-a".to_string(),
            subtask_id: "sub-1".to_string(),
            exit_reason: "completed".to_string(),
            kind: SettleKind::Completed,
            reply_target: None,
            caller: CallerIdentity::Agent,
        });

        // downcast せずに検証するため、具象型で1つ生成しても振る舞いを確認できる。
        let recording = RecordingSink::default();
        recording.on_subtask_settled(SubtaskSettled {
            session_id: "nostr-abc".to_string(),
            agent_id: "agent-b".to_string(),
            subtask_id: "sub-2".to_string(),
            exit_reason: "progress".to_string(),
            kind: SettleKind::Progress,
            reply_target: None,
            caller: CallerIdentity::Agent,
        });
        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subtask_id, "sub-2");
        assert_eq!(events[0].kind, SettleKind::Progress);
    }

    /// `settle_completed` は sink 発火の時点で subtask_completed ログが DB に
    /// 着地済みであること（順序契約 = RFC §6 受け入れ基準）を検証する。
    #[tokio::test]
    async fn settle_completed_persists_before_sink() {
        use std::sync::atomic::{AtomicI64, Ordering};

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());

        // 完了本体の代わりに、即完了しない pending task で abort_handle を用意。
        let handle = tokio::spawn(std::future::pending::<()>()).abort_handle();
        registry.insert(
            "sub-1".to_string(),
            SpawnedSubtask {
                abort_handle: handle,
                session_id: "subtask-sub-1".to_string(),
                parent_session_id: "discord-a-1-2".to_string(),
                agent_id: "agent-a".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: CallerIdentity::Agent,
                lifecycle: SubtaskLifecycle::new(),
            },
        );

        // sink は発火された瞬間の DB 上の subtask_completed ログ件数を記録する。
        struct OrderingSink {
            db: opencrab_db::Db,
            session_id: String,
            logs_at_fire: AtomicI64,
        }
        impl SubtaskCompletionSink for OrderingSink {
            fn on_subtask_settled(&self, _ev: SubtaskSettled) {
                let conn = self.db.lock().unwrap();
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
                        [&self.session_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                self.logs_at_fire.store(n, Ordering::SeqCst);
            }
        }
        let sink = OrderingSink {
            db: db.clone(),
            session_id: "discord-a-1-2".to_string(),
            logs_at_fire: AtomicI64::new(-1),
        };

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: "discord-a-1-2".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "sub-1".to_string(),
                sub_session_id: "subtask-sub-1".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "the result body",
        );

        // sink 発火時点で完了ログが既に DB にあった（DB 永続化 → 通知）。
        assert_eq!(sink.logs_at_fire.load(Ordering::SeqCst), 1);
        // registry からは除去済み。
        assert!(registry.is_empty());
    }

    /// 与えた `reply_target` で fake subtask を登録し、`settle_completed` を通した
    /// ときに sink が受け取った `SubtaskSettled` を返すヘルパ。
    ///
    /// 「DB 永続化 → registry 除去 → sink 発火」の順序契約もここで併せて検証する
    /// （sink 発火時点で完了ログが着地済み・registry から除去済み）。
    async fn settle_and_capture(reply_target: Option<&str>) -> SubtaskSettled {
        settle_and_capture_as(reply_target, CallerIdentity::Agent).await
    }

    /// `settle_and_capture` の呼び出し元指定版（#298）。
    async fn settle_and_capture_as(
        reply_target: Option<&str>,
        caller: CallerIdentity,
    ) -> SubtaskSettled {
        use std::sync::atomic::{AtomicI64, Ordering};

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "nostr-agent-a-npub1sender";

        let handle = tokio::spawn(std::future::pending::<()>()).abort_handle();
        registry.insert(
            "sub-rt".to_string(),
            SpawnedSubtask {
                abort_handle: handle,
                session_id: "subtask-sub-rt".to_string(),
                parent_session_id: parent.to_string(),
                agent_id: "agent-a".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: reply_target.map(|s| s.to_string()),
                caller,
                lifecycle: SubtaskLifecycle::new(),
            },
        );

        // sink 発火の瞬間に「完了ログ件数 / registry に残っているか」も記録する。
        struct CapturingSink {
            db: opencrab_db::Db,
            registry: SubtaskRegistry,
            session_id: String,
            event: Mutex<Option<SubtaskSettled>>,
            logs_at_fire: AtomicI64,
            still_registered: std::sync::atomic::AtomicBool,
        }
        impl SubtaskCompletionSink for CapturingSink {
            fn on_subtask_settled(&self, ev: SubtaskSettled) {
                let n: i64 = {
                    let conn = self.db.lock().unwrap();
                    conn.query_row(
                        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
                        [&self.session_id],
                        |row| row.get(0),
                    )
                    .unwrap()
                };
                self.logs_at_fire.store(n, Ordering::SeqCst);
                self.still_registered
                    .store(self.registry.contains_key(&ev.subtask_id), Ordering::SeqCst);
                *self.event.lock().unwrap() = Some(ev);
            }
        }

        let sink = CapturingSink {
            db: db.clone(),
            registry: registry.clone(),
            session_id: parent.to_string(),
            event: Mutex::new(None),
            logs_at_fire: AtomicI64::new(-1),
            still_registered: std::sync::atomic::AtomicBool::new(true),
        };

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: parent.to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "sub-rt".to_string(),
                sub_session_id: "subtask-sub-rt".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "the result body",
        );

        // 順序契約: sink 発火時点で DB 永続化済み・registry 除去済み。
        assert_eq!(
            sink.logs_at_fire.load(Ordering::SeqCst),
            1,
            "sink 発火より前に subtask_completed が DB へ着地している"
        );
        assert!(
            !sink.still_registered.load(Ordering::SeqCst),
            "sink 発火より前に registry から除去されている"
        );
        assert!(registry.is_empty());

        let captured = sink.event.lock().unwrap().take();
        captured.expect("sink が発火する")
    }

    /// #167: settle 時に registry の `reply_target` を読み出して sink へ渡す。
    /// 除去より前に回収するため、remove 後でも値が失われない。
    #[tokio::test]
    async fn settle_completed_passes_reply_target_to_sink() {
        let ev = settle_and_capture(Some("nostr:note1abcdef")).await;
        assert_eq!(ev.reply_target.as_deref(), Some("nostr:note1abcdef"));
        assert_eq!(ev.kind, SettleKind::Completed);
        assert_eq!(ev.subtask_id, "sub-rt");
    }

    /// #298: settle 時に registry の `caller`（= subtask を spawn した親 run の
    /// 呼び出し元）を読み出して sink へ渡す。resume する sink はこれで元の権限のまま
    /// 親ターンを再開できる。落とすと owner/trusted のツールが `policy_allows` で
    /// list_tools からも dispatch からも消える。
    #[tokio::test]
    async fn settle_completed_passes_caller_to_sink() {
        let ev = settle_and_capture_as(None, CallerIdentity::Owner).await;
        assert_eq!(
            ev.caller,
            CallerIdentity::Owner,
            "決着通知が呼び出し元を落としている（resume が最小権限へ降格する）"
        );

        // 昇格経路は作らない: 元が Agent なら Agent のまま。
        let ev = settle_and_capture_as(None, CallerIdentity::Agent).await;
        assert_eq!(ev.caller, CallerIdentity::Agent);
    }

    /// #167 非退行: `reply_target` が None（Discord 経路）なら None のまま渡り、
    /// 従来どおり session_id から返信先を復元する sink の挙動を変えない。
    #[tokio::test]
    async fn settle_completed_without_reply_target_yields_none() {
        let ev = settle_and_capture(None).await;
        assert!(ev.reply_target.is_none());
        assert_eq!(ev.exit_reason, "completed");
    }

    /// registry に該当エントリが無い（既に cancel された等）場合も従来どおり
    /// sink は発火し、`reply_target` は None になる。
    #[tokio::test]
    async fn settle_completed_missing_registry_entry_yields_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = RecordingSink::default();

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: "web-agent-a-c1".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "gone".to_string(),
                sub_session_id: "subtask-gone".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "body",
        );

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].reply_target.is_none());
    }

    /// 単一ツールを即完了（または永久 pending）で返す最小 executor。
    /// `SubtaskToolDispatcher` の配線検証用（合成 executor は別テストで検証済み）。
    struct FakeExecutor {
        pending: bool,
    }

    #[async_trait::async_trait]
    impl ActionExecutor for FakeExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            if self.pending {
                std::future::pending::<()>().await;
            }
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            Vec::new()
        }
    }

    /// #167: `SubtaskToolDispatcher::with_reply_target` の値が dispatch した
    /// subtask の `SpawnedSubtask.reply_target` に載る。
    #[tokio::test]
    async fn dispatcher_sets_reply_target_on_spawned_subtask() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            "nostr-agent-a-npub1sender",
        )
        .with_reply_target(Some("nostr:note1target".to_string()));

        let outcome = dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");
        let entry = registry.get(&outcome.subtask_id).unwrap();
        assert_eq!(entry.reply_target.as_deref(), Some("nostr:note1target"));
        entry.abort_handle.abort();
        drop(entry);
    }

    /// #167 非退行: `with_reply_target` を呼ばない（Discord 経路）と従来どおり None。
    #[tokio::test]
    async fn dispatcher_defaults_reply_target_to_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            "discord-agent-a-1-2",
        );

        let outcome = dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");
        let entry = registry.get(&outcome.subtask_id).unwrap();
        assert!(entry.reply_target.is_none());
        entry.abort_handle.abort();
        drop(entry);
    }

    /// #167: `RunRequest::with_reply_target` の値が（`process.rs` と同じ配線で）
    /// dispatcher → `SpawnedSubtask` → settle → sink まで一貫して運ばれる。
    #[tokio::test]
    async fn run_request_reply_target_reaches_sink_via_dispatcher() {
        use crate::traits::CallerIdentity;
        use crate::RunRequest;

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());

        // ゲートウェイ側（Nostr 等）が inbound の返信先を RunRequest に載せる。
        let req = RunRequest::new(
            "agent-a",
            "A",
            "nostr-agent-a-npub1sender",
            "sys",
            "conv",
            "nostr",
            CallerIdentity::Agent,
        )
        .with_reply_target("nostr:note1abcdef")
        .with_dispatch(Some(registry.clone()), sink.clone());
        assert_eq!(req.reply_target.as_deref(), Some("nostr:note1abcdef"));

        // process.rs の dispatcher 構築と同じ配線（RunRequest → dispatcher）。
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            req.agent_id.clone(),
            req.session_id.clone(),
        )
        .with_reply_target(req.reply_target.clone());

        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

        // settle（DB 永続化 → 除去 → sink 発火）まで待つ。
        for _ in 0..200 {
            if !sink.events.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].reply_target.as_deref(),
            Some("nostr:note1abcdef"),
            "RunRequest の reply_target が settle 時に sink まで届く"
        );
        assert_eq!(events[0].session_id, "nostr-agent-a-npub1sender");
    }

    /// #298: `RunRequest.caller` が（`process.rs` と同じ配線で）dispatcher →
    /// `SpawnedSubtask` → settle → sink まで一貫して運ばれる。
    ///
    /// 非ブロック dispatch は**普通のツール呼び出し**を background 化するので、
    /// オーナー発のターンでツールを 1 つ呼んだだけで resume が起きる。ここで
    /// 呼び出し元が落ちると、その resume 以降 owner/trusted のツールが丸ごと消える。
    #[tokio::test]
    async fn run_request_caller_reaches_sink_via_dispatcher() {
        use crate::RunRequest;

        for caller in [CallerIdentity::Owner, CallerIdentity::Agent] {
            let conn = opencrab_db::init_memory().unwrap();
            let db = opencrab_db::Db::from_connection(conn);
            let registry: SubtaskRegistry = Arc::new(DashMap::new());
            let sink = Arc::new(RecordingSink::default());

            let req = RunRequest::new(
                "agent-a",
                "A",
                "discord-agent-a-1-2",
                "sys",
                "conv",
                "discord",
                caller.clone(),
            )
            .with_dispatch(Some(registry.clone()), sink.clone());

            let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });
            let dispatcher = SubtaskToolDispatcher::new(
                executor,
                registry.clone(),
                db.clone(),
                sink.clone(),
                req.agent_id.clone(),
                req.session_id.clone(),
            )
            .with_caller(req.caller.clone());

            dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

            for _ in 0..200 {
                if !sink.events.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].caller, caller,
                "RunRequest の caller が settle 時に sink まで届いていない"
            );
        }
    }

    /// #167: `NoopCompletionSink` は sink 実装を書かずに `with_dispatch`
    /// （sink 必須 API）を満たせる。呼んでもログのみで何もしない。
    #[tokio::test]
    async fn noop_completion_sink_enables_dispatch_without_reinjection() {
        use crate::traits::CallerIdentity;
        use crate::RunRequest;

        let req = RunRequest::new(
            "agent-a",
            "A",
            "heartbeat-agent-a",
            "sys",
            "conv",
            "heartbeat",
            CallerIdentity::Agent,
        )
        .with_dispatch(None, Arc::new(NoopCompletionSink));
        // dispatch が有効化される（process.rs は completion_sink が Some のときだけ
        // dispatcher を注入する）。
        assert!(req.completion_sink.is_some());

        // dispatcher の sink として使え、settle まで通る（再注入はしない）。
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            req.completion_sink.clone().unwrap(),
            "agent-a",
            "heartbeat-agent-a",
        );
        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

        for _ in 0..200 {
            if registry.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(registry.is_empty(), "settle 後に registry から除去される");

        // 再注入はしないが、完了本文は DB へ永続化される（次 tick で拾える）。
        let conn = db.lock().unwrap();
        let logs =
            opencrab_db::queries::list_recent_session_logs(&conn, "heartbeat-agent-a", 10).unwrap();
        assert!(
            logs.iter().any(|l| l.content.contains("subtask_completed")),
            "NoopCompletionSink でも完了ログは DB へ着地する"
        );
    }

    /// RFC #152 S3a + S2 dormant 解消の実経路実証:
    /// dispatch した単一ツール（`nostr_generate_key`）が**合成 executor**
    /// （`BridgedExecutor` + gateway_actions = server ツール源）で実行され、完了が
    /// `settle_completed`（DB 永続化 → registry 除去 → sink 発火）で親セッションへ
    /// 再注入されること。
    #[tokio::test]
    async fn dispatched_single_tool_runs_on_composite_executor_and_reinjects() {
        use crate::bridge::BridgedExecutor;
        use crate::dispatcher::ActionDispatcher;
        use crate::traits::{ActionContext, CallerIdentity};
        use opencrab_gateway::{
            GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
        };

        // `nostr_generate_key` を提供する mock 合成 gateway（server ツール源の代役）。
        // nsec は返さず npub/pubkey のみ返す（実装と同じく秘密は LLM へ出さない）。
        struct MockServerGateway;
        #[async_trait::async_trait]
        impl GatewayActions for MockServerGateway {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![GatewayActionDef {
                    name: "nostr_generate_key".to_string(),
                    description: "generate a nostr key".to_string(),
                    parameters: serde_json::json!({"type":"object"}),
                }]
            }
            async fn execute(
                &self,
                name: &str,
                _args: &serde_json::Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                assert_eq!(name, "nostr_generate_key");
                GatewayActionResult {
                    success: true,
                    data: Some(serde_json::json!({"npub":"npub1abc","pubkey":"deadbeef"})),
                    error: None,
                }
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let parent = "discord-agent-x-1-2";
        let ctx = ActionContext {
            caller: CallerIdentity::Agent,
            agent_id: "agent-x".to_string(),
            agent_name: "X".to_string(),
            session_id: Some(parent.to_string()),
            db: db.clone(),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(Mutex::new(None)),
            model_override: Arc::new(Mutex::new(None)),
            current_purpose: Arc::new(Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "discord".to_string(),
            })),
        };

        // 合成 executor（gateway_actions に server ツール源）を 1 つの Arc にまとめる。
        let executor: Arc<dyn ActionExecutor> = Arc::new(
            BridgedExecutor::new(ActionDispatcher::new(), ctx)
                .with_gateway_actions(Arc::new(MockServerGateway)),
        );

        let sink = Arc::new(RecordingSink::default());
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-x",
            parent,
        );

        // dispatch 対象判定: server ツールは dispatch、制御系/配送系はしない。
        assert!(dispatcher.should_dispatch("nostr_generate_key"));
        assert!(!dispatcher.should_dispatch("spawn_subtask"));
        // 実在する Discord 配送系（`discord_send` は現行 gateway に無い死名だった）。
        assert!(!dispatcher.should_dispatch("discord_send_file"));
        assert!(!dispatcher.should_dispatch("send_ui"));
        // Nostr 配送系（#168）: background 化すると暗黙返信と二重投稿になる。
        assert!(!dispatcher.should_dispatch("nostr_reply"));
        assert!(!dispatcher.should_dispatch("nostr_post"));
        assert!(!dispatcher.should_dispatch("nostr_dm"));

        let outcome = dispatch_one(
            &dispatcher,
            "nostr_generate_key",
            serde_json::json!({}),
            "tc-1",
        );
        assert!(outcome.label.starts_with("nostr_generate_key("));

        // 完了待ち: settle_completed が registry から remove するまで。
        for _ in 0..200 {
            if registry.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(registry.is_empty(), "settle 後に registry から除去される");

        // sink が completed で 1 回だけ発火（再注入トリガ）。
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].exit_reason, "completed");
        assert_eq!(events[0].kind, SettleKind::Completed);
        assert_eq!(events[0].session_id, parent);
        drop(events);

        // DB へ subtask_completed が着地し、result にツール結果（npub）を含む
        // （resume は build_conversation_string でこれを読み直す = RFC §1.3）。
        let conn = db.lock().unwrap();
        let logs = opencrab_db::queries::list_recent_session_logs(&conn, parent, 10).unwrap();
        assert!(
            logs.iter().any(|l| {
                l.content.contains("subtask_completed") && l.content.contains("npub1abc")
            }),
            "親セッションログに subtask_completed（result=npub 含む）が永続化される"
        );
    }

    /// RFC #152 S3a / P0: auto-dispatch した subtask は**共有 registry** に載り、
    /// その `abort_handle` で停止できること（`cancel_subtask` の認可ゲートが叩く経路）。
    #[tokio::test]
    async fn dispatched_subtask_is_registered_and_abortable() {
        use crate::bridge::BridgedExecutor;
        use crate::dispatcher::ActionDispatcher;
        use crate::traits::{ActionContext, CallerIdentity};
        use opencrab_gateway::{
            GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
        };

        // 実行が完了しない（pending）ツールを提供する gateway。abort されるまで走り続ける。
        struct BlockingGateway;
        #[async_trait::async_trait]
        impl GatewayActions for BlockingGateway {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![GatewayActionDef {
                    name: "long_running".to_string(),
                    description: "never completes".to_string(),
                    parameters: serde_json::json!({"type":"object"}),
                }]
            }
            async fn execute(
                &self,
                _name: &str,
                _args: &serde_json::Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let parent = "discord-agent-x-1-2";
        let ctx = ActionContext {
            caller: CallerIdentity::Agent,
            agent_id: "agent-x".to_string(),
            agent_name: "X".to_string(),
            session_id: Some(parent.to_string()),
            db: db.clone(),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(Mutex::new(None)),
            model_override: Arc::new(Mutex::new(None)),
            current_purpose: Arc::new(Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "discord".to_string(),
            })),
        };
        let executor: Arc<dyn ActionExecutor> = Arc::new(
            BridgedExecutor::new(ActionDispatcher::new(), ctx)
                .with_gateway_actions(Arc::new(BlockingGateway)),
        );

        let sink = Arc::new(RecordingSink::default());
        // 共有 registry（ループと gateway_actions が共有するものの代役）。
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-x",
            parent,
        );

        let outcome = dispatch_one(&dispatcher, "long_running", serde_json::json!({}), "tc-1");

        // 共有 registry に載っている（＝cancel_subtask から到達可能）。
        assert!(registry.contains_key(&outcome.subtask_id));
        let entry = registry.get(&outcome.subtask_id).unwrap();
        assert_eq!(entry.parent_session_id, parent);

        // cancel_subtask 相当: abort_handle で停止 → registry から除去。
        entry.abort_handle.abort();
        drop(entry);
        registry.remove(&outcome.subtask_id);

        // 完了で settle しないので sink は発火しない（aborted = 完了イベント無し）。
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(registry.is_empty());
        assert!(
            sink.events.lock().unwrap().is_empty(),
            "abort された subtask は settle_completed を通らず sink を発火しない"
        );
    }

    /// 与えた parent_session_id で「即完了しない」fake subtask を registry へ登録し、
    /// その JoinHandle を返す（abort されたか検証するため）。
    fn insert_fake_subtask(
        registry: &SubtaskRegistry,
        subtask_id: &str,
        parent_session_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert(
            subtask_id.to_string(),
            SpawnedSubtask {
                abort_handle: handle.abort_handle(),
                session_id: format!("subtask-{subtask_id}"),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "agent-a".to_string(),
                label: "long job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: CallerIdentity::Agent,
                lifecycle: SubtaskLifecycle::new(),
            },
        );
        handle
    }

    /// #161: 親セッションからの cancel_subtask は abort + 除去し、親ログへ
    /// tool_cancelled を記録する。
    #[tokio::test]
    async fn cancel_subtask_parent_session_aborts_and_removes() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "web-agent-a-conv1";
        let handle = insert_fake_subtask(&registry, "st-1", parent);

        let outcome = cancel_subtask(&registry, &db, None, None, "st-1", false, Some(parent));
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert!(registry.is_empty(), "cancel 後に registry から除去される");
        // 実際に abort された。
        assert!(handle.await.unwrap_err().is_cancelled());
        // 親セッションログに tool_cancelled が着地する。
        let conn = db.lock().unwrap();
        let logs = opencrab_db::queries::list_recent_session_logs(&conn, parent, 10).unwrap();
        assert!(
            logs.iter().any(|l| l.log_type == "tool_cancelled"),
            "親ログに tool_cancelled が記録される"
        );
    }

    /// #161: 存在しない subtask_id は NotFound（権限拒否ではない）。
    #[tokio::test]
    async fn cancel_subtask_missing_is_not_found() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let outcome = cancel_subtask(&registry, &db, None, None, "nope", false, Some("web-a-c1"));
        assert_eq!(outcome, CancelOutcome::NotFound);
    }

    /// #161: 他セッションが親の subtask は Unauthorized で拒否し、abort もしない。
    #[tokio::test]
    async fn cancel_subtask_foreign_session_unauthorized() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-x", "web-other-c9");

        let outcome = cancel_subtask(&registry, &db, None, None, "st-x", false, Some("web-me-c1"));
        assert_eq!(outcome, CancelOutcome::Unauthorized);
        // 拒否したのでエントリは残り、abort もされない。
        assert!(registry.contains_key("st-x"));
        handle.abort(); // テスト後始末。
    }

    /// #161: session 文脈が無い agent は他人の subtask を停止できない（Unauthorized）。
    #[tokio::test]
    async fn cancel_subtask_no_session_unauthorized() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-ns", "web-other-c9");

        let outcome = cancel_subtask(&registry, &db, None, None, "st-ns", false, None);
        assert_eq!(outcome, CancelOutcome::Unauthorized);
        assert!(registry.contains_key("st-ns"));
        handle.abort();
    }

    /// #161: owner は無関係なセッション文脈からでも停止できる。
    #[tokio::test]
    async fn cancel_subtask_owner_bypasses_session_check() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-any", "web-other-c9");

        let outcome = cancel_subtask(&registry, &db, None, None, "st-any", true, None);
        assert_eq!(outcome, CancelOutcome::Cancelled);
        assert!(registry.is_empty());
        assert!(handle.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn registry_holds_spawned_subtask() {
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = tokio::spawn(async {
            // 即完了せず abort_handle を有効に保つ。
            std::future::pending::<()>().await;
        })
        .abort_handle();

        let entry = SpawnedSubtask {
            abort_handle: handle,
            session_id: "sub-session-1".to_string(),
            parent_session_id: "discord-123".to_string(),
            agent_id: "agent-a".to_string(),
            label: "compile the report".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: Some("channel:456".to_string()),
            caller: CallerIdentity::Agent,
            lifecycle: SubtaskLifecycle::new(),
        };
        registry.insert("sub-1".to_string(), entry);

        assert_eq!(registry.len(), 1);
        let got = registry.get("sub-1").unwrap();
        assert_eq!(got.parent_session_id, "discord-123");
        assert_eq!(got.reply_target.as_deref(), Some("channel:456"));

        // abort して registry から除去（cancel 相当）。
        got.abort_handle.abort();
        drop(got);
        registry.remove("sub-1");
        assert!(registry.is_empty());
    }

    // -----------------------------------------------------------------------
    // レビュー指摘（P0/P1）の回帰テスト群
    // -----------------------------------------------------------------------

    /// 親セッションログに着地した subtask_completed の件数。
    fn completed_log_count(db: &opencrab_db::Db, session_id: &str) -> usize {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_recent_session_logs(&conn, session_id, 50)
            .unwrap()
            .iter()
            .filter(|l| l.content.contains("subtask_completed"))
            .count()
    }

    /// 親セッションログの subtask_completed 本文（最初の 1 件）。
    fn completed_log_body(db: &opencrab_db::Db, session_id: &str) -> String {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_recent_session_logs(&conn, session_id, 50)
            .unwrap()
            .into_iter()
            .find(|l| l.content.contains("subtask_completed"))
            .map(|l| l.content)
            .unwrap_or_default()
    }

    /// settle が終わる（registry が空になる）まで待つ。
    async fn wait_until_settled(registry: &SubtaskRegistry) {
        for _ in 0..400 {
            if registry.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("subtask が決着しなかった（registry が空にならない）");
    }

    /// ラッチ: 停止と決着は排他（先に主張した一方だけが成功する）。
    #[test]
    fn lifecycle_claims_are_mutually_exclusive() {
        let l = SubtaskLifecycle::new();
        assert!(l.claim_cancel());
        assert!(!l.claim_settle(), "cancel 済みなら settle は主張できない");
        assert!(l.is_cancelled());

        let l2 = SubtaskLifecycle::new();
        assert!(l2.claim_settle());
        assert!(!l2.claim_cancel(), "決着済みなら cancel は主張できない");
        assert!(l2.is_settling());
    }

    /// [P0 回帰] cancel が先に主張していたら `settle_completed` は
    /// **DB 記録も sink 発火もしない**（止めたのに返信が届くのを防ぐ）。
    #[tokio::test]
    async fn settle_after_cancel_persists_nothing_and_fires_no_sink() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = RecordingSink::default();
        let parent = "web-agent-a-conv1";

        let lifecycle = SubtaskLifecycle::new();
        assert!(lifecycle.claim_cancel(), "cancel が先に主張する");

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: parent.to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "st-1".to_string(),
                sub_session_id: "subtask-st-1".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle,
            },
            "the result body",
        );

        assert_eq!(
            sink.events.lock().unwrap().len(),
            0,
            "cancel 後は完了 sink を発火しない"
        );
        assert_eq!(
            completed_log_count(&db, parent),
            0,
            "cancel 後は subtask_completed を DB へ書かない"
        );
    }

    /// [P0 回帰] 実経路: ツールが**完走した直後**（settle の DB 永続化より前）に
    /// `cancel_subtask` が入っても、完了ログも sink 発火も起きない。
    ///
    /// 競合窓を決定的に再現するため、executor が結果を返す直前に自分で
    /// `cancel_subtask` を呼ぶ（= tool 完走 → cancel → settle の順序）。
    #[tokio::test]
    async fn cancel_in_settle_window_suppresses_completion() {
        /// 結果を返す直前に自分の subtask を cancel する executor。
        struct CancellingExecutor {
            registry: SubtaskRegistry,
            db: opencrab_db::Db,
            outcome: Arc<Mutex<Option<CancelOutcome>>>,
        }
        #[async_trait::async_trait]
        impl ActionExecutor for CancellingExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                // 走行中の自分（registry の唯一のエントリ）を停止する。
                let id = self
                    .registry
                    .iter()
                    .next()
                    .map(|e| e.key().clone())
                    .expect("dispatch した subtask が registry にある");
                let outcome = cancel_subtask(&self.registry, &self.db, None, None, &id, true, None);
                *self.outcome.lock().unwrap() = Some(outcome);
                ActionResult {
                    success: true,
                    data: serde_json::json!({"ok": true}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                Vec::new()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let cancel_outcome = Arc::new(Mutex::new(None));

        let executor: Arc<dyn ActionExecutor> = Arc::new(CancellingExecutor {
            registry: registry.clone(),
            db: db.clone(),
            outcome: cancel_outcome.clone(),
        });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

        wait_until_settled(&registry).await;
        // settle 側が走り切るのを待つ（発火するならこの間に発火する）。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            *cancel_outcome.lock().unwrap(),
            Some(CancelOutcome::Cancelled),
            "cancel は成功を返している"
        );
        assert_eq!(
            sink.events.lock().unwrap().len(),
            0,
            "cancel 成功後に完了 sink が発火してはならない（resume して返信が届く）"
        );
        assert_eq!(
            completed_log_count(&db, parent),
            0,
            "cancel 成功後に subtask_completed が DB へ書かれてはならない"
        );
    }

    /// [P1 回帰] cancel は完了経路ではなく `on_subtask_cancelled` を通り、
    /// `exit_reason="cancelled"` / `kind=Cancelled` で通知される
    /// （REST が最後の subtask 停止でセッションを完了にできる）。
    #[tokio::test]
    async fn cancel_notifies_sink_without_completion() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = RecordingSink::default();
        let parent = "agent-msg-agent-a-u1";
        let handle = insert_fake_subtask(&registry, "st-1", parent);

        let outcome = cancel_subtask(
            &registry,
            &db,
            Some(&sink),
            None,
            "st-1",
            false,
            Some(parent),
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);

        let cancelled = sink.cancelled.lock().unwrap();
        assert_eq!(cancelled.len(), 1, "停止は sink へ 1 回通知される");
        assert_eq!(cancelled[0].exit_reason, "cancelled");
        assert_eq!(cancelled[0].kind, SettleKind::Cancelled);
        assert_eq!(cancelled[0].session_id, parent);
        // 完了経路（resume する側）は発火しない。
        assert!(
            sink.events.lock().unwrap().is_empty(),
            "停止で on_subtask_settled（resume 経路）を呼んではならない"
        );
        assert!(registry.is_empty());
        handle.abort();
    }

    /// [P0 回帰] 同一バッチの複数ツールは 1 subtask 内で**dispatch 順に逐次実行**され、
    /// 完了 sink は **1 回だけ**発火する（N 通の返信にならない）。
    #[tokio::test]
    async fn batch_runs_sequentially_in_order_and_settles_once() {
        /// 実行順を記録し、最初のツールだけ遅い executor
        /// （個別 spawn だと速い方が先に完走して順序が崩れる）。
        struct OrderExecutor {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl ActionExecutor for OrderExecutor {
            async fn execute(&self, name: &str, _args: &serde_json::Value) -> ActionResult {
                if name == "slow_tool" {
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                }
                self.order.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!({"tool": name}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                Vec::new()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let order = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn ActionExecutor> = Arc::new(OrderExecutor {
            order: order.clone(),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );

        let calls = vec![
            DispatchCall {
                tool_name: "slow_tool".to_string(),
                args: serde_json::json!({"path": "x"}),
                tool_call_id: "tc-1".to_string(),
            },
            DispatchCall {
                tool_name: "fast_tool".to_string(),
                args: serde_json::json!({"cmd": "build"}),
                tool_call_id: "tc-2".to_string(),
            },
        ];
        let outcome = dispatcher.dispatch_batch(&calls);
        // バッチ全体で subtask は 1 本だけ。
        assert_eq!(registry.len(), 1, "1 バッチ = 1 subtask");
        assert!(outcome.label.contains("slow_tool"));
        assert!(outcome.label.contains("fast_tool"));

        wait_until_settled(&registry).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["slow_tool", "fast_tool"],
            "遅い方が先に dispatch されていれば先に実行される（並行化して順序を失わない）"
        );
        assert_eq!(
            sink.events.lock().unwrap().len(),
            1,
            "1 親ターンの resume は 1 回だけ"
        );
        assert_eq!(
            completed_log_count(&db, parent),
            1,
            "完了ログもバッチにつき 1 件"
        );
        // 本文には両ツールの結果が入る（resume 時に DB から読み直される）。
        let body = completed_log_body(&db, parent);
        assert!(body.contains("slow_tool") && body.contains("fast_tool"));
    }

    /// 指定名のツールだけ永久に pending する executor（残りは即成功）。
    /// 完走した call 数を数える（cancel 時の部分結果検証用）。
    struct HangingExecutor {
        hang_on: String,
        finished: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ActionExecutor for HangingExecutor {
        async fn execute(&self, name: &str, _args: &serde_json::Value) -> ActionResult {
            if name == self.hang_on {
                std::future::pending::<()>().await;
            }
            self.finished.lock().unwrap().push(name.to_string());
            ActionResult {
                success: true,
                data: serde_json::json!({"tool": name}),
                error: None,
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            Vec::new()
        }
    }

    fn call(tool: &str, id: &str) -> DispatchCall {
        DispatchCall {
            tool_name: tool.to_string(),
            args: serde_json::json!({"x": 1}),
            tool_call_id: id.to_string(),
        }
    }

    /// [P2 回帰] timeout でバッチが打ち切られたとき、**未実行 call も本文に現れる**。
    ///
    /// system prompt は「同じツールを再呼びするな（もう走っている）」と指示するので、
    /// 痕跡が無いとエージェントは未実行を知る手段が無く依頼が無言で消える。
    #[tokio::test]
    async fn timed_out_batch_records_skipped_calls() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(HangingExecutor {
            hang_on: "hangs".to_string(),
            finished: Arc::new(Mutex::new(Vec::new())),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_timeout(std::time::Duration::from_millis(60));

        dispatcher.dispatch_batch(&[
            call("ok1", "tc-1"),
            call("hangs", "tc-2"),
            call("ok2", "tc-3"),
            call("ok3", "tc-4"),
        ]);
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        // 4 call すべてが本文に現れる（未実行の 2 つは skipped として）。
        for id in ["tc-1", "tc-2", "tc-3", "tc-4"] {
            assert!(body.contains(id), "{id} が完了本文に無い: {body}");
        }
        assert_eq!(
            body.matches("skipped: batch timed out").count(),
            2,
            "未実行 call（ok2 / ok3）が skipped として記録されるべき: {body}"
        );
        assert_eq!(sink.events.lock().unwrap()[0].exit_reason, "timeout");
    }

    /// [P2 回帰] 複数ツールバッチの完了本文は **構造として** 結果を埋め、
    /// `tool_call_id` を含む（三重エスケープと順序依存の対応付けの解消）。
    #[tokio::test]
    async fn batch_body_embeds_results_as_json_with_tool_call_id() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(HangingExecutor {
            hang_on: "never".to_string(),
            finished: Arc::new(Mutex::new(Vec::new())),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        // 同じツールを 2 回呼ぶ（順序でしか対応が取れなかったケース）。
        dispatcher.dispatch_batch(&[call("ws_write", "tc-a"), call("ws_write", "tc-b")]);
        wait_until_settled(&registry).await;

        // 完了ログ本文（`{"type":"subtask_completed",...,"result":"<配列 JSON>"}`）を
        // 2 段でパースし、結果が**文字列ではなく object** であることを確かめる。
        let log: serde_json::Value =
            serde_json::from_str(&completed_log_body(&db, parent)).expect("完了ログは JSON");
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(log["result"].as_str().expect("result は配列 JSON 文字列"))
                .expect("result は JSON 配列としてパースできる");
        assert_eq!(arr.len(), 2);
        for (i, id) in ["tc-a", "tc-b"].iter().enumerate() {
            assert_eq!(arr[i]["tool"], "ws_write");
            assert_eq!(
                arr[i]["tool_call_id"], *id,
                "tool_call_id が無いと同名ツールの対応が取れない: {arr:?}"
            );
            assert!(
                arr[i]["result"].is_object(),
                "結果は構造として埋める（文字列だと多重エスケープになる）: {}",
                arr[i]["result"]
            );
            assert_eq!(arr[i]["result"]["success"], true);
        }
    }

    /// [P2 回帰] cancel でバッチを止めたとき、**完走済み call の部分結果**が親ログに残る
    /// （どこまで進んだかがラベルしか残らないのを防ぐ）。
    #[tokio::test]
    async fn cancel_records_partial_results_of_completed_calls() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let finished = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn ActionExecutor> = Arc::new(HangingExecutor {
            hang_on: "hangs".to_string(),
            finished: finished.clone(),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        let outcome = dispatcher.dispatch_batch(&[
            call("ws_write", "tc-1"),
            call("ws_write", "tc-2"),
            call("hangs", "tc-3"),
        ]);

        // 先頭 2 call が完走してハングに入るまで待つ。
        for _ in 0..200 {
            if finished.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(finished.lock().unwrap().len(), 2);

        let cancelled = cancel_subtask(
            &registry,
            &db,
            Some(sink.as_ref()),
            None,
            &outcome.subtask_id,
            true,
            None,
        );
        assert_eq!(cancelled, CancelOutcome::Cancelled);

        // 完了 sink は発火しない（停止したので返信しない）が、部分結果は残る。
        assert!(sink.events.lock().unwrap().is_empty());
        let conn = db.lock().unwrap();
        let log = opencrab_db::queries::list_recent_session_logs(&conn, parent, 10)
            .unwrap()
            .into_iter()
            .find(|l| l.log_type == "tool_cancelled")
            .expect("tool_cancelled が親ログに残る");
        assert!(
            log.content.contains("2 completed tool call(s)"),
            "完走済み call 数が残るべき: {}",
            log.content
        );
        assert!(log.content.contains("tc-1") && log.content.contains("tc-2"));
        assert!(
            !log.content.contains("tc-3"),
            "未完了 call は部分結果に含めない: {}",
            log.content
        );
        let meta: serde_json::Value =
            serde_json::from_str(log.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(meta["completed_calls"].as_array().unwrap().len(), 2);
    }

    /// [P0 回帰] dispatch にもタイムアウトがあり、`exit_reason="timeout"` で
    /// settle して registry から除去される（永久滞留＝無言の消失を防ぐ）。
    #[tokio::test]
    async fn dispatch_times_out_and_settles() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        // 完了しないツール。
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_timeout(std::time::Duration::from_millis(60));

        dispatch_one(&dispatcher, "hangs_forever", serde_json::json!({}), "tc-1");
        wait_until_settled(&registry).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].exit_reason, "timeout",
            "既定タイムアウト超過は exit_reason=timeout で到達する"
        );
        assert_eq!(completed_log_count(&db, parent), 1);
    }

    /// 既定のタイムアウトは `spawn_subtask` と揃える。
    #[test]
    fn default_dispatch_timeout_matches_spawn_subtask() {
        assert_eq!(DEFAULT_DISPATCH_TIMEOUT_SECS, 1800);
    }

    /// [P1 回帰] ツールが panic しても `exit_reason="error"` で settle され、
    /// registry に死骸が残らない（REST が永久 active にならない）。
    #[tokio::test]
    async fn dispatch_panic_settles_as_error() {
        struct PanicExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for PanicExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                panic!("boom inside tool");
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                Vec::new()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(PanicExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        dispatch_one(&dispatcher, "panics", serde_json::json!({}), "tc-1");

        wait_until_settled(&registry).await;
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1, "panic でも settle して通知する");
        assert_eq!(events[0].exit_reason, "error");
        assert!(completed_log_body(&db, parent).contains("panicked"));
    }

    /// [P1 回帰] dispatch 経路も inline と同じ無害化を通す:
    /// 大きい結果はワークスペースへ退避し、DB にはメタ情報だけを残す（#294）。
    #[tokio::test]
    async fn dispatch_offloads_large_result_like_inline() {
        struct BigExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for BigExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                ActionResult {
                    success: true,
                    data: serde_json::json!({"blob": "Z".repeat(50_000)}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                Vec::new()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let dir = tempfile::TempDir::new().unwrap();
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(BigExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_workspace_root(Some(dir.path().to_path_buf()));

        dispatch_one(&dispatcher, "read_file", serde_json::json!({}), "tc-big");
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        assert!(
            body.contains("Tool result withheld") && body.contains("tmp/"),
            "上限超過はファイルへ退避してメタ情報だけ残す: {}",
            &body[..body.len().min(300)]
        );
        assert!(
            !body.contains("ZZZ"),
            "巨大本文（プレビューを含む）が session_logs に入ってはならない"
        );
        assert!(dir.path().join("tmp").read_dir().unwrap().count() > 0);
    }

    /// [P1 回帰] dispatch 経路でも秘密鍵はマスクして永続化する。
    #[tokio::test]
    async fn dispatch_redacts_secret_result_like_inline() {
        struct SecretExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for SecretExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                ActionResult {
                    success: true,
                    data: serde_json::json!({"npub": "npub1ok", "nsec": "nsec1leaked"}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                Vec::new()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(SecretExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        dispatch_one(
            &dispatcher,
            "nostr_generate_key",
            serde_json::json!({}),
            "tc-1",
        );
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        assert!(
            !body.contains("nsec1leaked"),
            "秘密鍵が DB へ入ってはならない"
        );
        assert!(body.contains("redacted"));
        assert!(body.contains("npub1ok"), "非秘密は保持する");
    }

    /// [P1 回帰] run 内共有状態を書く `select_llm` は dispatch しない（inline のまま）。
    #[test]
    fn select_llm_is_not_dispatched() {
        let set = default_non_dispatch_tools();
        assert!(
            set.contains("select_llm"),
            "select_llm は run 内共有状態（model_override）を書くため inline に残す"
        );
    }

    /// [P1 回帰 / fail-closed] `ActionDispatcher` の core アクション**全名**が
    /// inline / dispatch のどちらかに分類されている（#152）。
    ///
    /// これが無かった頃は core 32 個が分類ガードの外にあり全 dispatch されていた:
    /// 記憶想起フロー（`search_memory_index` → `retrieve_memory_nodes`）が背景往復
    /// 2 回 = ユーザーへ 4 通、`open_task` は task_id の代わりに `spawned` が返る、
    /// という壊れ方をしていた。既存の `pure_read_tools_are_not_dispatched` は Discord
    /// gateway の読み取りしか見ないので検知できなかった。
    #[test]
    fn core_actions_are_classified_for_dispatch() {
        let names = crate::dispatcher::ActionDispatcher::new().action_names();
        assert!(
            !names.is_empty(),
            "core アクションが 1 つも登録されていない"
        );

        for name in &names {
            let inline = crate::bridge::CORE_INLINE_ACTIONS.contains(&name.as_str());
            let dispatchable = crate::bridge::CORE_DISPATCHABLE_ACTIONS.contains(&name.as_str());
            assert!(
                inline ^ dispatchable,
                "core アクション {name} が未分類（または両方に居る）。\
                 新しいアクションを登録したら CORE_INLINE_ACTIONS か \
                 CORE_DISPATCHABLE_ACTIONS のどちらかへ入れること（判定基準は \
                 default_non_dispatch_tools の doc / docs/DESIGN.md）"
            );
        }
        // 死名検出: 一覧側に実在しない名前を残さない（空振りする分類を防ぐ）。
        for name in crate::bridge::CORE_INLINE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "CORE_INLINE_ACTIONS の {name} が ActionDispatcher に無い（死名）"
            );
        }
        for name in crate::bridge::CORE_DISPATCHABLE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "CORE_DISPATCHABLE_ACTIONS の {name} が ActionDispatcher に無い（死名）"
            );
        }
        assert_eq!(
            names.len(),
            crate::bridge::CORE_INLINE_ACTIONS.len()
                + crate::bridge::CORE_DISPATCHABLE_ACTIONS.len(),
            "分類の総数が登録アクション数と一致しない"
        );

        // 分類が実際に効いている（除外集合へ反映されている）。
        let non_dispatch = default_non_dispatch_tools();
        for name in crate::bridge::CORE_INLINE_ACTIONS {
            assert!(
                non_dispatch.contains(*name),
                "{name} は inline 分類なのに dispatch されてしまう"
            );
        }
        for name in crate::bridge::CORE_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} は dispatch 可分類なのに inline 集合に居る"
            );
        }
    }

    /// [P1 回帰] system prompt が指示する記憶想起フローと台帳の同ターン連鎖は inline。
    /// dispatch されると 1 質問が複数ターン・複数メッセージに割れる。
    #[test]
    fn memory_recall_and_task_ledger_tools_are_inline() {
        let set = default_non_dispatch_tools();
        for name in [
            // 記憶想起（2 段連鎖）。
            "search_memory_index",
            "retrieve_memory_nodes",
            "browse_memory_index",
            // 純粋読み取り。
            "ws_read",
            "ws_list",
            "get_task",
            "read_skill",
            "get_system_info",
            // 同ターン結果依存（戻り値の task_id を後続で使う）。
            "open_task",
        ] {
            assert!(
                set.contains(name),
                "{name} は inline でなければならない（分類基準 3/5）"
            );
        }
    }

    /// [P1 回帰] MCP ツール（`mcp__*`）は既定 inline。運用者が繋いだ任意ツールの性質
    /// （配送系か / 同ターン結果依存か）は静的に分類できないため安全側に倒す。
    #[tokio::test]
    async fn mcp_tools_are_not_dispatched_by_default() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry,
            db,
            sink,
            "agent-a",
            "web-agent-a-conv1",
        );

        assert!(!dispatcher.should_dispatch("mcp__slack__post_message"));
        assert!(!dispatcher.should_dispatch("mcp__anything__at_all"));
        // 非 MCP の dispatch 可ツールは従来どおり dispatch される。
        assert!(dispatcher.should_dispatch("execute_shell"));
        assert!(dispatcher.should_dispatch("ws_write"));
    }

    /// 分類集合の内部整合性（#152）。
    ///
    /// - inline 集合と dispatch 可リストは互いに素（同じ名前が両方に属さない）。
    /// - `DISCORD_ACTIONS`（depth ゲート）は inline 集合の部分集合。配送系を depth
    ///   ゲートに入れておきながら dispatch してしまう食い違いを防ぐ。
    /// - inline 集合は重複を含まない（一覧の手編集で二重に足す事故の検出）。
    #[test]
    fn dispatch_classification_sets_are_consistent() {
        let non_dispatch = default_non_dispatch_tools();

        for name in crate::bridge::CORE_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} が dispatch 可リストと inline 集合の両方に居る"
            );
        }
        for name in crate::bridge::DISCORD_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} が dispatch 可リストと inline 集合の両方に居る"
            );
        }
        for name in crate::bridge::NOSTR_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} が dispatch 可リストと inline 集合の両方に居る"
            );
        }
        for name in crate::bridge::SERVER_DISPATCHABLE_ACTIONS {
            assert!(
                !non_dispatch.contains(*name),
                "{name} が dispatch 可リストと inline 集合の両方に居る"
            );
        }
        // `DISCORD_ACTIONS`（depth ゲート）は **inline 集合全体**の部分集合。
        // `send_ui` のように実装が gateway 非依存層へ移った名前は
        // `SERVER_INLINE_ACTIONS` 側に属するため、`DISCORD_INLINE_ACTIONS` 単体ではなく
        // `default_non_dispatch_tools()` の和集合で判定する（不変条件は「depth ゲートに
        // 載る名前は dispatch されない」であって、どの定数に属するかではない）。
        for name in crate::bridge::DISCORD_ACTIONS {
            assert!(
                non_dispatch.contains(*name),
                "{name} は depth ゲート（DISCORD_ACTIONS）にあるのに dispatch されてしまう"
            );
        }
        let unique: HashSet<&&str> = crate::bridge::DISCORD_INLINE_ACTIONS.iter().collect();
        assert_eq!(
            unique.len(),
            crate::bridge::DISCORD_INLINE_ACTIONS.len(),
            "DISCORD_INLINE_ACTIONS に重複がある"
        );
        let unique: HashSet<&&str> = crate::bridge::CORE_INLINE_ACTIONS.iter().collect();
        assert_eq!(
            unique.len(),
            crate::bridge::CORE_INLINE_ACTIONS.len(),
            "CORE_INLINE_ACTIONS に重複がある"
        );
        let unique: HashSet<&&str> = crate::bridge::SERVER_INLINE_ACTIONS.iter().collect();
        assert_eq!(
            unique.len(),
            crate::bridge::SERVER_INLINE_ACTIONS.len(),
            "SERVER_INLINE_ACTIONS に重複がある"
        );
    }

    /// [P1 回帰] server 内蔵の設定ツール（transport 非依存で web/REST/heartbeat の
    /// 全ターンに載る）は inline。分類ガードの外にあった頃は 5 個が background 化され、
    /// 設定変更（LLM ルーターのホットスワップ等）と一覧取得が同ターンで返らなかった。
    #[test]
    fn server_config_tools_are_not_dispatched() {
        let set = default_non_dispatch_tools();
        for name in [
            "configure_llm_provider",
            "manage_allowed_commands",
            "configure_nostr",
            "configure_self",
            "configure_mcp_server",
            "cancel_subtask",
        ] {
            assert!(
                set.contains(name),
                "{name} は server の設定ツール（共有状態の書き込み / 純粋な読み取り）なので inline"
            );
        }
        // 長時間の鍵探索だけは dispatch 対象に残す。
        assert!(!set.contains("nostr_generate_key"));
    }

    /// [P1 回帰] 配送系 + ユーザー応答待ちの `send_ui` は dispatch しない。
    /// background 化すると UI 投稿と本文返信の順序が入れ替わり、クリック resume と
    /// subtask 決着 resume で返信が 2 通になる。
    #[test]
    fn send_ui_is_not_dispatched() {
        assert!(default_non_dispatch_tools().contains("send_ui"));
    }

    /// [P1 回帰] 戻り値の URL を同ターンで使う `ensure_*webhook` は dispatch しない。
    #[test]
    fn same_turn_result_dependent_tools_are_not_dispatched() {
        let set = default_non_dispatch_tools();
        for name in [
            "ensure_webhook",
            "ensure_subtask_webhook",
            "discord_create_webhook",
            "discord_create_channel",
            "nostr_upload",
        ] {
            assert!(
                set.contains(name),
                "{name} は同ターンで戻り値を使うため inline"
            );
        }
    }

    /// [P1 回帰] 純粋な読み取りは dispatch しない（質問 1 つが 2 ターンに割れる）。
    #[test]
    fn pure_read_tools_are_not_dispatched() {
        let set = default_non_dispatch_tools();
        for name in [
            "list_webhooks",
            "list_subtask_webhooks",
            "list_allowed_commands",
            "get_default_webhook",
            "get_default_subtask_webhook",
            "read_heartbeat_instructions",
            "discord_list_channels",
            "discord_list_guilds",
        ] {
            assert!(set.contains(name), "{name} は純粋な読み取りなので inline");
        }
    }

    /// 長時間処理は dispatch 対象に**残る**（除外集合が広がりすぎた回帰の検出）。
    #[test]
    fn long_running_tools_stay_dispatchable() {
        let set = default_non_dispatch_tools();
        for name in ["nostr_generate_key", "rebuild_memory_index", "create_skill"] {
            assert!(
                !set.contains(name),
                "{name} は長時間処理なので dispatch 対象に残す"
            );
        }
    }
}
