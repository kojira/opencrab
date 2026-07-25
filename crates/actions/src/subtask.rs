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
use std::sync::Arc;

use dashmap::DashMap;
use opencrab_core::{
    ActionExecutor, ActionResult, DispatchOutcome, FunctionDefinition, ToolDispatcher,
};
use tokio::task::AbortHandle;

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
/// 既存 `opencrab_discord::SpawnedSubtask` と同型だが、Discord 固有の
/// webhook フィールド（`WebhookConfig` / `DeliveryBatch`）は持たない。
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
    /// 起動時刻（duration 算出用）。monotonic な `Instant` を用いる。
    pub started_at: std::time::Instant,
    /// gateway 不透明な返信ルーティング token（spawn 時に捕捉）。
    /// settle 時にランタイムが registry から引いて sink へ渡す。
    /// `None` なら返信配送しない。
    pub reply_target: Option<String>,
}

/// アクティブな subtask を subtask_id で引く registry（gateway 非依存版）。
///
/// 現 `opencrab_discord::SubtaskRegistry` と同型だが gateway 非依存。
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

    // 2. registry から除去し、除去したエントリから reply_target を回収する。
    //    remove 後は引けないため、remove の戻り値から読み出す（#167）。
    let reply_target = registry
        .remove(&ctx.subtask_id)
        .and_then(|(_, subtask)| subtask.reply_target);

    // 3. sink を発火する（本文は運ばない = DB 永続化済み）。
    sink.on_subtask_settled(SubtaskSettled {
        session_id: ctx.parent_session_id,
        agent_id: ctx.agent_id,
        subtask_id: ctx.subtask_id,
        exit_reason: ctx.exit_reason,
        kind: SettleKind::Completed,
        reply_target,
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

/// 走行中 subtask を停止する中核処理（gateway 非依存 / #161）。
///
/// web / Nostr / REST など Discord 以外の transport でも `cancel_subtask` ツールを
/// 露出できるよう、認可・abort・registry 除去・親ログ記録を server-neutral 層へ
/// 集約する。Discord の `execute_cancel_subtask` と同じ契約を踏襲する。
///
/// 認可（#64）: `is_owner` なら常に許可。そうでなければ「呼び出し元セッションが親
/// （`parent_session_id == caller_session_id`）」の subtask のみ停止できる（自己/兄弟/
/// 他セッションのものは不可）。`remove_if` は shard ロック下で述語を評価するため、
/// 「認可確認 → 削除」の間にエントリが差し替わる TOCTOU が無い（所有権フィールドは
/// insert 後不変）。
///
/// 成功時: `abort_handle.abort()` → registry から除去 → 親セッションログへ
/// `tool_cancelled` を best-effort 記録する。abort により background closure は
/// 中断されるため `settle_completed`（完了 sink 発火）は通らない = 完了イベント無し。
pub fn cancel_subtask(
    registry: &SubtaskRegistry,
    db: &opencrab_db::Db,
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

    match registry.remove_if(subtask_id, |_, s| authorized(s)) {
        Some((_, subtask)) => {
            subtask.abort_handle.abort();

            // 親セッションログへ subtask_cancelled を best-effort 記録する。
            let parent = subtask.parent_session_id.clone();
            if !parent.is_empty() {
                if let Ok(conn) = db.lock() {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: subtask.agent_id.clone(),
                        session_id: parent,
                        log_type: "tool_cancelled".to_string(),
                        content: format!("subtask '{}' was cancelled", subtask.label),
                        speaker_id: None,
                        turn_number: None,
                        metadata_json: Some(
                            serde_json::json!({
                                "tool_call_id": subtask_id,
                                "tool_name": "spawn_subtask",
                                "label": subtask.label,
                            })
                            .to_string(),
                        ),
                        created_at: None,
                    };
                    opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
                }
            }
            CancelOutcome::Cancelled
        }
        None => {
            // remove_if の None は「不在」と「権限なし」の両方。所有権フィールドは
            // insert 後不変なので contains_key で区別できる。
            if registry.contains_key(subtask_id) {
                CancelOutcome::Unauthorized
            } else {
                CancelOutcome::NotFound
            }
        }
    }
}

// ---------------------------------------------------------------------------
// S3a: 単一ツールを subtask として実行する dispatcher（非ブロック / 全ツール自動化）
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
/// - 制御系（`spawn_subtask` / `cancel_subtask` / `report_progress`）: それ自体が
///   subtask ライフサイクルを操作するため background 化しない。
/// - 配送系（Discord 送信・VC 参加/退出・peer review 依頼・Nostr 送信）: 「送る」こと
///   自体が応答であり、background 化して完了で再注入する意味がない。加えて gateway が
///   「明示送信したか」を親ターンの終わりに見て暗黙返信を抑制する場合（Nostr）、
///   background 化は**二重投稿**を生む。
///
/// 呼び出し側は `SubtaskToolDispatcher::with_non_dispatch` で上書き/追加できる。
pub fn default_non_dispatch_tools() -> HashSet<String> {
    let mut set: HashSet<String> = ["spawn_subtask", "cancel_subtask", "report_progress"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Discord 配送系（bridge の DISCORD_ACTIONS と同一集合）。
    for name in crate::bridge::DISCORD_ACTIONS {
        set.insert((*name).to_string());
    }
    // Nostr 配送系（#168）。background 化すると親ターンが「明示送信済み」フラグを
    // 観測できず、暗黙返信と二重投稿になる。`nostr_generate_key` は含まない
    // （長時間処理なので dispatch 対象に残す）。
    for name in crate::bridge::NOSTR_DELIVERY_ACTIONS {
        set.insert((*name).to_string());
    }
    set
}

/// 「単一ツールを background subtask として実行する」job のランタイム（RFC #152 S3a）。
///
/// `execute_spawn_subtask` が sub-engine（LLM ループ）を建てるのに対し、これは
/// **指定 1 ツールを合成 executor で実行するだけ**の job。spawn / registry 登録
/// （`SpawnedSubtask`, label=`tool(主要引数)`）/ 完了時 `settle_completed`（DB 永続化
/// → registry 除去 → sink 発火）という中核は既存と共有し、job の中身だけ差し替える。
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
        }
    }

    /// auto-dispatch 対象外の集合を差し替える。
    pub fn with_non_dispatch(mut self, non_dispatch: HashSet<String>) -> Self {
        self.non_dispatch = non_dispatch;
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

impl ToolDispatcher for SubtaskToolDispatcher {
    fn should_dispatch(&self, tool_name: &str) -> bool {
        !self.non_dispatch.contains(tool_name)
    }

    fn dispatch(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        tool_call_id: &str,
    ) -> DispatchOutcome {
        let subtask_id = uuid::Uuid::new_v4().to_string();
        let sub_session_id = format!("subtask-{subtask_id}");
        let label = dispatch_label(tool_name, args);

        // 各種クローン（background タスクへムーブ）。
        let executor = self.executor.clone();
        let registry = self.registry.clone();
        let db = self.db.clone();
        let sink = self.sink.clone();
        let agent_id = self.agent_id.clone();
        let parent_session_id = self.parent_session_id.clone();
        let tool_name_owned = tool_name.to_string();
        let args_owned = args.clone();
        let tool_call_id_owned = tool_call_id.to_string();
        let subtask_id_task = subtask_id.clone();
        let sub_session_id_task = sub_session_id.clone();

        // 開始ゲート: 親が registry へ insert し終えるまでタスク本体を走らせない
        // （即完了する job が親の insert より先に remove して running のままリークするのを防ぐ。
        //  execute_spawn_subtask と同じ不変条件）。
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            let _ = start_rx.await;

            // 単一ツールを合成 executor で実行する（sub-engine は建てない）。
            let result = executor
                .execute_with_id(&tool_name_owned, &args_owned, &tool_call_id_owned)
                .await;
            let exit_reason = if result.success { "completed" } else { "error" };
            // 完了本文 = ツール結果 JSON（DB へ永続化。sink には運ばない = RFC §1.3）。
            let result_text = serde_json::to_string(&result)
                .unwrap_or_else(|_| r#"{"success":false}"#.to_string());

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
                started_at: std::time::Instant::now(),
                reply_target: self.reply_target.clone(),
            },
        );
        // insert 完了 → タスク本体の実行を許可する。
        let _ = start_tx.send(());

        DispatchOutcome { subtask_id, label }
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
    }

    impl SubtaskCompletionSink for RecordingSink {
        fn on_subtask_settled(&self, ev: SubtaskSettled) {
            self.events.lock().unwrap().push(ev);
        }
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
                started_at: std::time::Instant::now(),
                reply_target: None,
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
                started_at: std::time::Instant::now(),
                reply_target: reply_target.map(|s| s.to_string()),
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

        let outcome = dispatcher.dispatch("some_tool", &serde_json::json!({}), "tc-1");
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

        let outcome = dispatcher.dispatch("some_tool", &serde_json::json!({}), "tc-1");
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

        dispatcher.dispatch("some_tool", &serde_json::json!({}), "tc-1");

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
        dispatcher.dispatch("some_tool", &serde_json::json!({}), "tc-1");

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
        assert!(!dispatcher.should_dispatch("discord_send"));
        // Nostr 配送系（#168）: background 化すると暗黙返信と二重投稿になる。
        assert!(!dispatcher.should_dispatch("nostr_reply"));
        assert!(!dispatcher.should_dispatch("nostr_post"));
        assert!(!dispatcher.should_dispatch("nostr_dm"));

        let outcome = dispatcher.dispatch("nostr_generate_key", &serde_json::json!({}), "tc-1");
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

        let outcome = dispatcher.dispatch("long_running", &serde_json::json!({}), "tc-1");

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
                started_at: std::time::Instant::now(),
                reply_target: None,
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

        let outcome = cancel_subtask(&registry, &db, "st-1", false, Some(parent));
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
        let outcome = cancel_subtask(&registry, &db, "nope", false, Some("web-a-c1"));
        assert_eq!(outcome, CancelOutcome::NotFound);
    }

    /// #161: 他セッションが親の subtask は Unauthorized で拒否し、abort もしない。
    #[tokio::test]
    async fn cancel_subtask_foreign_session_unauthorized() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let handle = insert_fake_subtask(&registry, "st-x", "web-other-c9");

        let outcome = cancel_subtask(&registry, &db, "st-x", false, Some("web-me-c1"));
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

        let outcome = cancel_subtask(&registry, &db, "st-ns", false, None);
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

        let outcome = cancel_subtask(&registry, &db, "st-any", true, None);
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
            started_at: std::time::Instant::now(),
            reply_target: Some("channel:456".to_string()),
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
}
