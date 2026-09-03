//! #925 V3 レーンの時刻発火受け口（TimedFire heartbeat）。
//!
//! heartbeat ターンは「発端 said の無い自己ターン」で、V3 の resume ターン
//! （[`crate::completion::run_v3_said_less_turn`]）と完全に同型。ここには **wire を 1 byte も
//! 変えずに** heartbeat を V3 レーンへ載せるための最小 2 部品だけを置く:
//!
//! 1. [`ExtgateFire`]: `extgate-<binding_id>` の発火先を名乗る静的 descriptor（[`TransportFire`]）。
//! 2. [`ExtgateTimedFireSink`]: 受けて session→binding を解決し、生きた binding 内で resume と
//!    同じ said 無しターンを 1 本回す薄い sink（[`TimedFireSink`]）。
//!
//! 新経路・新プロトコル・新設定は 0。配送・ロック・記録・継続・🏁 は全て既存の V3 機構
//! （`turn_queues` / `session_locks` / `ExtgateCompletionSink` / `apply_delivery_effect` /
//! `emit_activity` / `select_completed_target`）を再利用する。

use std::sync::Arc;

use opencrab_actions::{
    AgentRuntime, FireTarget, TimedFireRequest, TimedFireSink, TransportFire, TransportFireEnv,
};

use crate::completion::{run_v3_said_less_turn, ExtgateCompletionSink, EXTGATE_SESSION_PREFIX};
use crate::inbound::{resolve_binding_context, BindingContext};
use crate::registry::ExtgateState;

/// extgate レーンの時刻発火 kind（sink 登録簿のキー・[`FireTarget::kind`]）。
///
/// instance の `kind_id`（"discord" / "nostr"）とは別の**発火ルーティング kind**で、canonical
/// session `extgate-<binding_id>` を単一 descriptor で受けるためのもの。Discord も Nostr も V3 では
/// この 1 本の kind に載る（session が両 transport とも `extgate-<binding_id>`・`gate_binding.rs`
/// `canonical_session_id`）。
pub const EXTGATE_TIMED_FIRE_KIND: &str = "extgate";

/// `extgate-<binding_id>` の発火先を名乗る descriptor（#628 / #925）。
///
/// **性質**: live G マスタゲートの対象外（`is_g_gated=false`・Nostr / web と同じ）。応答本文は
/// `delivery_mode`（instance 設定・既定 say）に従って gateway へ配送される（Discord=チャンネル投稿 /
/// Nostr=タイムライン投稿）。誘導文言は transport 非依存（§1.7・`posts_response_body` は撤去済み）。
pub struct ExtgateFire;

impl TransportFire for ExtgateFire {
    fn kind(&self) -> &'static str {
        EXTGATE_TIMED_FIRE_KIND
    }

    /// canonical extgate session `extgate-<binding_id>` を自分の発火先として名乗る。
    ///
    /// session は **binding 主権**（agent 非依存）。`agent_id` は使わない（discord/nostr descriptor
    /// と違い接頭辞に agent を含まない）。binding_id は UUID なので、壊れた session_id では
    /// `None`（fail-closed・外部へ発火を捏造しない）。`route` に binding_id を載せて
    /// [`build_session_id`](Self::build_session_id) の逆写像を成立させる。
    fn parse(&self, session_id: &str, _agent_id: &str) -> Option<FireTarget> {
        let binding_id = session_id.strip_prefix(EXTGATE_SESSION_PREFIX)?;
        // fail-closed: binding_id は canonical UUID。壊れた値では名乗らない。
        uuid::Uuid::parse_str(binding_id).ok()?;
        Some(FireTarget {
            kind: EXTGATE_TIMED_FIRE_KIND,
            channel_id: String::new(),
            guild_id: String::new(),
            route: binding_id.to_string(),
        })
    }

    /// [`parse`](Self::parse) の逆写像。`extgate-<binding_id>` を組む（agent 非依存）。
    fn build_session_id(&self, target: &FireTarget, _agent_id: &str) -> String {
        format!("{EXTGATE_SESSION_PREFIX}{}", target.route)
    }

    fn is_g_gated(&self) -> bool {
        false
    }

    fn human_hint(&self) -> &'static str {
        "ゲートに接続したセッション"
    }

    /// この extgate 受信ゲートウェイが設定上「立ち上がるべき」か（実行時述語・条件 D）。
    ///
    /// gate socket が設定されていれば V3 レーンは立ち上がる。server は `gate_socket` が
    /// あるとき自分の kind を `configured_shared_kinds` へ畳んで渡す（main.rs）。
    fn should_be_running(&self, env: &TransportFireEnv) -> bool {
        env.configured_shared_kinds
            .contains(&EXTGATE_TIMED_FIRE_KIND)
    }

    fn sample_target(&self) -> FireTarget {
        FireTarget {
            kind: EXTGATE_TIMED_FIRE_KIND,
            channel_id: String::new(),
            guild_id: String::new(),
            route: "11111111-1111-4111-8111-111111111111".to_string(),
        }
    }
}

/// `extgate-<binding_id>` を **生きた binding** へ解決する（§1.5・fail-loud）。
///
/// 解決できない（binding 不明 / closed / instance 未削除でない / instance が live でない /
/// この binding を acknowledged していない / config 壊れ）ときは `None`。呼び出し側はその場合
/// 発火を諦めて warn を残す（新規投稿の捏造や別 binding への誤送はしない）。
///
/// binding→(instance/kind/agent/owner/delivery_mode) の解決は allowlisted な
/// [`crate::inbound::resolve_binding_context`] へ委譲する（owner の platform 別解決という
/// platform 語彙をこの共有ファイルへ持ち込まないため）。live 判定は platform 非依存なのでここで行う。
fn resolve_live_binding(
    state: &ExtgateState,
    session_id: &str,
) -> Option<(String, BindingContext)> {
    let binding_id = session_id.strip_prefix(EXTGATE_SESSION_PREFIX)?.to_string();
    // DB 段（open binding・未削除 instance・owner・delivery_mode）。db ロックは registry ロックより先に手放す。
    let ctx = {
        let conn = state.db.lock().ok()?;
        resolve_binding_context(&conn, &binding_id)?
    };
    // live 判定（§1.5・DESIGN-gateway-takein-v2:173 fail-loud）: instance が live かつ
    // この binding を acknowledged していること。未接続なら None（＝warn・無配送）。
    {
        let reg = state.registry.lock().ok()?;
        let live = reg.get(&ctx.instance_id)?;
        if !live.acknowledged.contains(&binding_id) {
            return None;
        }
    }
    Some((binding_id, ctx))
}

/// V3 の時刻発火受け口（self-driven sink）。受けて生きた binding 内で resume と同じ said 無し
/// ターンを 1 本回すだけ（薄く保つ）。非ブロック（scheduler を塞がない）。
pub struct ExtgateTimedFireSink<R: AgentRuntime> {
    state: Arc<ExtgateState>,
    runtime: R,
}

impl<R: AgentRuntime> ExtgateTimedFireSink<R> {
    pub fn new(state: Arc<ExtgateState>, runtime: R) -> Self {
        Self { state, runtime }
    }
}

impl<R: AgentRuntime> TimedFireSink for ExtgateTimedFireSink<R> {
    fn fire_timed_turn(&self, req: TimedFireRequest) {
        // §1.5 fail-loud: 生きた binding を解決できないなら**発火せず**この 1 箇所だけで warn を残す
        // （resolve 段でここへ落ちたら return するので delivery 段の warn と二重にならない＝session_id
        // ごとちょうど 1 件）。`session_id` は **plain &str フィールド**で載せる（Display/Debug 経路だと
        // 引用符が付き、観測側の等値照合がずれるため）。「gateway なしのハートビートは存在しない」
        // （DIRECTION-LOG 478）の実装。
        let (binding_id, ctx) = match resolve_live_binding(&self.state, &req.session_id) {
            Some(r) => r,
            None => {
                tracing::warn!(
                    session_id = req.session_id.as_str(),
                    agent_id = req.agent_id.as_str(),
                    "timed-fire(extgate): 生きた binding を解決できない（gateway 未接続 / binding 不明・closed）。発火を諦める（fail-loud・投稿を捏造しない）"
                );
                return;
            }
        };
        // resume と同じ sink を組む。prompt は system suffix（会話ログに発言として残さない）。
        // author_id は owner（自己ターン）。reply_target は発端 origin が無いので None（standalone）。
        let sink = ExtgateCompletionSink {
            state: Arc::clone(&self.state),
            runtime: self.runtime.clone(),
            instance_id: ctx.instance_id,
            binding_id,
            agent_id: ctx.agent_id,
            session_id: req.session_id,
            kind_id: ctx.kind_id,
            author_id: ctx.owner_id,
            delivery_mode: ctx.delivery_mode,
            prompt_suffix: req.prompt,
        };
        let caller = req.caller;
        tokio::spawn(async move {
            // heartbeat は subtask 完了を発端にしないので consumed_completion は None。
            run_v3_said_less_turn(sink, caller, None, None).await;
        });
    }
}
