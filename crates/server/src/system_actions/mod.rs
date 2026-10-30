//! エージェント（owner 限定）が OpenCrab 自体の設定を変更するためのサーバ内ツール源。
//!
//! `AppState`（db / llm_router / llm_config）を必要とするため、素の dispatcher
//! アクションでは配線できない。`GatewayActions` として実装し、`BridgedExecutor` の
//! 単一 `gateway_actions` スロットに載せる。既存の gateway（Discord/Nostr 等）を
//! `inner` として保持し、自分が扱わないツールは委譲する（composite）ことで、
//! transport 非依存に「設定ツール」を全ターンへ供給する。
//!
//! owner ゲートは bridge の `OWNER_ONLY_ACTIONS`（可視性 + 実行の双方）が担うが、
//! 多層防御として本ハンドラでも caller を確認する（fail-closed）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use opencrab_actions::{
    cancel_subtask as neutral_cancel_subtask, steer_subtask as neutral_steer_subtask,
    CancelOutcome, SettleKind, SteerOutcome, SubtaskCompletionSink, SubtaskRegistry,
    SubtaskSettled, REJECTION_CODE_PREFIX,
};
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use opencrab_mcp::is_valid_server_name;
use serde_json::{json, Value};

use crate::AppState;

/// `report_progress` のデバウンス待機時間。Discord 実装（`execute_report_progress`）と同一。
///
/// この時間内に後続の `report_progress` が来たら世代が進み、古い方は発火しない。
const PROGRESS_DEBOUNCE_DELAY: Duration = Duration::from_secs(3);

/// `configure_llm_provider` などのサーバ内設定ツールを提供する `GatewayActions`。
pub struct SystemGatewayActions {
    state: AppState,
    /// transport 固有の gateway（Discord/Nostr 等）。自分が扱わないツールを委譲する。
    inner: Option<Arc<dyn GatewayActions>>,
    /// auto-dispatch した走行中 subtask の共有 registry（#161）。web/Nostr/REST でも
    /// `cancel_subtask` を露出するため server-neutral 層に配線する。`run_agent_response`
    /// が dispatcher へ渡すものと同一 Arc（Discord では gateway_actions の registry とも
    /// 同一）。`None` の場合は走行中 subtask が無く cancel は not found を返す。
    subtask_registry: Option<SubtaskRegistry>,
    /// 停止（`cancel_subtask`）を通知する完了 sink（この run の `RunRequest` と同一）。
    ///
    /// 停止は `on_subtask_cancelled`（既定は no-op）で通知するため resume は起きない。
    /// REST のように「最後の subtask の決着でセッションを完了にする」経路は、この通知
    /// を受けて `sessions.status` の整合を取る（無いと永久 `active` のまま残る）。
    completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
    /// D-1091: この run の binding が宣言した環境変数（`RunRequest::run_env` と同一）。
    ///
    /// `spawn_subtask` が起こす sub-run は**親ターンと同じ binding** の作業なので、
    /// 親の宣言をそのまま引き継ぐ（引き継がないと、サブタスクの `execute_shell` だけ
    /// 別アカウントの環境で走る）。空なら従来どおり何もしない。
    run_env: std::collections::BTreeMap<String, String>,
}

/// `report_progress` が登録簿から引く、進捗通知に要る項目だけの写し。
///
/// 登録簿のエントリ（`SpawnedSubtask`）は shard ロック下でしか読めないので、必要な
/// フィールドをここへ写してからロックを離す。
struct ProgressSubtaskEntry {
    /// 解決済みの subtask ID（引数省略時は session_id からの逆引き結果）。
    subtask_id: String,
    /// subtask 自身のセッション ID（所有権ゲート用）。
    session_id: String,
    /// 親セッション ID（進捗ログと resume の宛先）。
    parent_session_id: String,
    /// registry の人間可読ラベル（D-1087 のライフサイクル通知に載せる）。
    label: String,
    /// **親ターンの呼び出し元**（#298）。進捗デバウンス発火は親会話を resume する
    /// ので、resume 先の権限は元のターンのものでなければならない。
    caller: opencrab_actions::CallerIdentity,
}

impl ProgressSubtaskEntry {
    fn from_entry(subtask_id: String, entry: &opencrab_actions::SpawnedSubtask) -> Self {
        Self {
            subtask_id,
            session_id: entry.session_id.clone(),
            parent_session_id: entry.parent_session_id.clone(),
            label: entry.label.clone(),
            caller: entry.caller.clone(),
        }
    }
}

impl SystemGatewayActions {
    pub fn new(
        state: AppState,
        inner: Option<Arc<dyn GatewayActions>>,
        subtask_registry: Option<SubtaskRegistry>,
        completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
    ) -> Self {
        Self {
            state,
            inner,
            subtask_registry,
            completion_sink,
            run_env: std::collections::BTreeMap::new(),
        }
    }

    /// D-1091: この run の binding が宣言した環境（`RunRequest::run_env`）を覚える。
    ///
    /// `build_turn_executor` が run ごとに組み立てる 1 箇所からだけ呼ぶ。プロセス全体の
    /// 設定へは置かないので、binding の環境が別の run へ漏れない。
    pub(crate) fn with_run_env(
        mut self,
        run_env: std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.run_env = run_env;
        self
    }

    /// 本ツール源が直接提供するツール定義。
    ///
    /// 各定義は分類属性（`class.dispatch` / `class.sub_engine` / `class.sharing`）を
    /// 名乗る（`ToolClass` に `Default` が無いため構築サイトで必須）。テストの
    /// 分類検査がこの全量から属性を引くので `pub(crate)`。
    pub(crate) fn own_definitions() -> Vec<GatewayActionDef> {
        Self::always_own_definitions()
    }
}

include!("definitions/configuration.rs");
include!("definitions/subtasks_progress.rs");
include!("definitions/memory_commands_skill.rs");
include!("definitions/heartbeat_schedules.rs");
include!("definitions.rs");

include!("execution/subtasks_memory.rs");
include!("execution/configuration.rs");
include!("gateway_actions.rs");

#[cfg(test)]
mod tests;

/// #412: `configure_self` から未登録モデルを設定できないこと。
///
/// オーナーが会話で「モデルを変えて」と言う経路がここ。ダッシュボードの口
/// （`PUT`/`PATCH /api/agents/{id}`）だけ塞いでも、こちらが素通りなら
/// 「黙って既定値」状態は同じように再発する。
#[cfg(test)]
mod configure_self_model_gate_tests;
