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
    /// transport が提供する A2UI 描画面（#156 S3）。`inner` から 1 度だけ引く。
    ///
    /// `send_ui` の実体は gateway 非依存層（`opencrab_actions::a2ui`）にあるが、描画と
    /// ユーザー応答の受け取りは transport にしか作れない。`Some` のときだけ `send_ui`
    /// を露出する（描画できない transport のターンに「必ず失敗するツール」を出さない）。
    a2ui: Option<Arc<opencrab_core::a2ui::A2uiSurface>>,
    /// transport が提供する素テキストの配送口（#157 S7）。`inner` から 1 度だけ引く。
    ///
    /// `request_peer_review` の実体は gateway 非依存層（`crate::peer_review`）にあるが、
    /// 宛先検査・メンション記法・1 通の上限・送信そのものは transport にしか作れない。
    /// `a2ui` と違い**露出は絞らない**（配送口の無い transport でも定義に出す）: ツールが
    /// transport の有無で消えないようにするのが #157 の目的で、無いときは実行だけが
    /// 明示エラーになる。
    text_delivery: Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>>,
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
        let a2ui = inner.as_ref().and_then(|i| i.a2ui_surface());
        let text_delivery = inner.as_ref().and_then(|i| i.text_delivery());
        Self {
            state,
            inner,
            subtask_registry,
            completion_sink,
            a2ui,
            text_delivery,
        }
    }

    /// 本ツール源が直接提供するツール定義（A2UI 描画面がある構成の全量）。
    ///
    /// 各定義は分類属性（`class.dispatch` / `class.sub_engine` / `class.sharing`）を
    /// 名乗る（`ToolClass` に `Default` が無いため構築サイトで必須）。テストや
    /// `agent_heartbeat` の分類検査がこの全量から属性を引くので `pub(crate)`。
    pub(crate) fn own_definitions() -> Vec<GatewayActionDef> {
        let mut defs = Self::always_own_definitions();
        defs.push(opencrab_actions::send_ui_definition());
        defs
    }

    /// `with_a2ui` が false のときは `send_ui` を落とす。
    ///
    /// `send_ui` は A2UI を描画できる transport（現状 Discord）のターンだけに出す。
    /// 移設前は `DiscordGatewayActions::definitions()` にしか無かったので、これで
    /// 露出範囲が移設前と一致する。
    fn own_definitions_with_a2ui(with_a2ui: bool) -> Vec<GatewayActionDef> {
        let mut defs = Self::own_definitions();
        if !with_a2ui {
            defs.retain(|d| d.name != "send_ui");
        }
        defs
    }
}

include!("definitions/configuration.rs");
include!("definitions/nostr.rs");
include!("definitions/subtasks_progress.rs");
include!("definitions/memory_commands_skill.rs");
include!("definitions/heartbeat_schedules.rs");
include!("definitions/webhooks.rs");
include!("definitions/peer_review.rs");
include!("definitions.rs");

include!("execution/nostr.rs");
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
