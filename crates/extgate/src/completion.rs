//! V3 turn の subtask 決着 sink。既存の `with_dispatch` / `run_session_turn` を再利用する。
//!
//! Discord / Nostr / REST は `RunRequest::with_dispatch` を付ける。V3（extgate inbound）
//! だけが付けていなかったため、`process.rs` が dispatcher を刺さず SkillEngine が
//! 全ツールを `execute_with_id` で同期実行していた。

use std::sync::Arc;

use opencrab_actions::{
    delivery_effect, run_session_turn, AgentRuntime, CallerIdentity, LiveInboundScope, RunRequest,
    SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled,
};

use crate::delivery::apply_delivery_effect;
use crate::delivery_mode::{adjust_inbound_effect, DeliveryMode};
use crate::listen::emit_activity;
use crate::registry::ExtgateState;

/// `session_id_for_binding` と同じ接頭辞。`dispatch_settled` が親セッション判定に使う。
pub const EXTGATE_SESSION_PREFIX: &str = "extgate-";

/// #915 §13.3.5: activity ended に載せる `completed_target`（🏁 の付け先＝最終生成の最後の投稿の
/// 発話 id）を選ぶ。通常ターン（`inbound::enqueue_turn`）と resume ターン（`resume_v3_turn`）で
/// 規則は共通なので 1 実装に集約する（単一実装）。**送る＝🏁 を付ける／`None`＝付けない**。
///
/// - `engine_completion`: `(最終生成の最後の投稿系 utterance-op の call_id, stopped_by_limit,
///   最終生成が CONTINUE 本文を配送したか)`。engine を回さなかったターンは `None`。
/// - `started_subtask` / `agent_has_running`: 進行中があれば付けない（idle でない・§13.3.1 案E）。
/// - `final_say_id`: 最終応答が say を配送したときのその delivery_id。
/// - `last_continuation_say`: 上限打ち切りで最終生成が CONTINUE 本文を配送したとき、その say の
///   delivery_id（呼び出し側が Mutex から解決して渡す）。
pub(crate) fn select_completed_target(
    engine_completion: Option<(Option<String>, bool, bool)>,
    started_subtask: bool,
    agent_has_running: bool,
    final_say_id: Option<String>,
    last_continuation_say: Option<String>,
) -> Option<String> {
    if started_subtask || agent_has_running {
        return None;
    }
    engine_completion.and_then(|(last_reply, stopped_by_limit, final_had_speech)| {
        if stopped_by_limit {
            if final_had_speech {
                last_continuation_say
            } else {
                last_reply
            }
        } else {
            final_say_id.or(last_reply)
        }
    })
}

/// V3 の `RunRequest` に既存の `with_dispatch` を常時付ける。ノブ分岐は置かない。
///
/// `process.rs` の `auto_dispatch` は触らない（撤去は別 PR）。sink が無いと
/// dispatcher が刺さらず、SkillEngine が全ツールを同期実行する。
pub fn v3_attach_dispatch(
    mut req: RunRequest,
    kind_id: &str,
    author_id: impl Into<String>,
    registry: SubtaskRegistry,
    sink: Arc<dyn SubtaskCompletionSink>,
) -> RunRequest {
    req = req.with_dispatch(Some(registry), sink);
    if kind_id == "nostr" {
        req = req.with_live_inbound_scope(LiveInboundScope::OnlySpeaker(author_id.into()));
    }
    req
}

/// 決着を親セッションの `run_session_turn` へ戻す。配送は既存の `apply_delivery_effect`。
pub struct ExtgateCompletionSink<R: AgentRuntime> {
    pub state: Arc<ExtgateState>,
    pub runtime: R,
    pub instance_id: String,
    pub binding_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub kind_id: String,
    pub author_id: String,
    pub delivery_mode: DeliveryMode,
    pub prompt_suffix: String,
}

impl<R: AgentRuntime> Clone for ExtgateCompletionSink<R> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            runtime: self.runtime.clone(),
            instance_id: self.instance_id.clone(),
            binding_id: self.binding_id.clone(),
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            kind_id: self.kind_id.clone(),
            author_id: self.author_id.clone(),
            delivery_mode: self.delivery_mode,
            prompt_suffix: self.prompt_suffix.clone(),
        }
    }
}

impl<R: AgentRuntime> SubtaskCompletionSink for ExtgateCompletionSink<R> {
    fn session_prefix(&self) -> &'static str {
        EXTGATE_SESSION_PREFIX
    }

    /// 接頭辞ではなく **sink が実際に握る session** と等値比較する。
    ///
    /// extgate の session は `canonical_session_id` 由来で、Nostr の再利用セッションでは
    /// binding（`extgate-<id>`）ではなく address（`nostr-<agent_id>`）になる。接頭辞
    /// `extgate-` 前提の既定判定だと、その決着を全て門前払いして resume が起きなかった
    /// （#838 row284）。この sink は spawn 時に握った `session_id` を保持しているので、
    /// それと一致するかで判定すれば取り違えずに継続できる。
    fn owns_parent_session(&self, session_id: &str) -> bool {
        session_id == self.session_id
    }

    fn forwards_progress(&self) -> bool {
        false
    }

    fn deliver_continuation(&self, ev: SubtaskSettled) {
        let sink = self.clone();
        tokio::spawn(async move {
            resume_v3_turn(sink, ev).await;
        });
    }
}

async fn resume_v3_turn<R: AgentRuntime>(sink: ExtgateCompletionSink<R>, ev: SubtaskSettled) {
    // resume は発端 said の無い自己ターン。heartbeat（#925）と完全に同型なので共有ヘルパへ
    // 委譲する（単一実装）。resume は発端 origin への返信先（`ev.reply_target`）を持ち回るが、
    // heartbeat 側は origin が無いので `None`（standalone post）を渡す。
    run_v3_said_less_turn(sink, ev.caller.clone(), ev.reply_target.clone()).await;
}

/// 発端 said の無い自己ターン（resume 継続 / #925 heartbeat）を 1 本駆動する共有実装。
///
/// `caller` は実行権限（resume は spawn 時の caller・heartbeat は `Owner`）。`reply_target` は
/// 発端 origin への返信先（resume は spawn 時に捕捉した origin・heartbeat は `None`＝standalone
/// post）。started（origin=None・👀 は付けない）→ system＋`prompt_suffix` → `run_session_turn`
/// （新規 said を記録しない）→ 継続（`v3_attach_dispatch`＋`ExtgateCompletionSink`）→ 配送
/// （`apply_delivery_effect`）→ 🏁（`select_completed_target`）を、通常ターンと同じ機構で回す。
pub(crate) async fn run_v3_said_less_turn<R: AgentRuntime>(
    sink: ExtgateCompletionSink<R>,
    caller: CallerIdentity,
    reply_target: Option<String>,
) {
    let locks = sink.runtime.session_locks();
    let session_id = sink.session_id.clone();
    locks
        .run_serialized(&session_id, async move {
            let activity_id = uuid::Uuid::new_v4().to_string();
            emit_activity(
                &sink.state,
                &sink.instance_id,
                &sink.binding_id,
                &activity_id,
                "started",
                None,
                None,
            )
            .await;
            let (system, name) = sink.runtime.build_agent_context(&sink.agent_id, &caller);
            let system = if sink.prompt_suffix.is_empty() {
                system
            } else {
                format!("{system}\n\n{}", sink.prompt_suffix)
            };
            let registry = sink.runtime.subtask_registry_for(&sink.session_id);
            let dispatch: Arc<dyn SubtaskCompletionSink> = Arc::new(sink.clone());
            let kind_id = sink.kind_id.clone();
            let author_id = sink.author_id.clone();
            let subtask_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let last_continuation_say = Arc::new(std::sync::Mutex::new(None::<String>));
            // DI 拡張 §8: resume ターンでも宣言能力を tool set へ載せる（連鎖して更に DI 操作を
            // 呼べるように）。宣言が無ければ None。
            let ops_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> =
                crate::ops_projection::ExtgateOpsGatewayActions::for_binding(
                    Arc::clone(&sink.state),
                    &sink.instance_id,
                    &sink.binding_id,
                    &sink.session_id,
                    &sink.agent_id,
                )
                .map(|a| Arc::new(a) as Arc<dyn opencrab_gateway::GatewayActions>);
            let turn = run_session_turn(
                &sink.runtime,
                &sink.session_id,
                &sink.agent_id,
                &system,
                "",
                |raw| raw.to_string(),
                |conversation| {
                    let mut req = RunRequest::new(
                        sink.agent_id.clone(),
                        name.clone(),
                        sink.session_id.clone(),
                        system.clone(),
                        conversation,
                        "extgate",
                        caller.clone(),
                    )
                    .with_subtask_starts(Arc::clone(&subtask_starts))
                    // #930: resume ターンで畳み込んだ said にも read+origin を付ける（付与規則は
                    // 主ターンと同一・1 origin 1 回）。同時に origin を畳み込み済みに記録し独立ターンを抑止。
                    .with_on_read_origin({
                        let state = Arc::clone(&sink.state);
                        let instance_id = sink.instance_id.clone();
                        let binding_id = sink.binding_id.clone();
                        let session_id = sink.session_id.clone();
                        Arc::new(move |origin: String| {
                            let state = Arc::clone(&state);
                            let instance_id = instance_id.clone();
                            let binding_id = binding_id.clone();
                            let session_id = session_id.clone();
                            Box::pin(async move {
                                let activity_id = uuid::Uuid::new_v4().to_string();
                                crate::listen::emit_activity(
                                    &state,
                                    &instance_id,
                                    &binding_id,
                                    &activity_id,
                                    "read",
                                    Some(origin.as_str()),
                                    None,
                                )
                                .await;
                                state.mark_folded(&session_id, &origin);
                            })
                        })
                    })
                    .with_on_continuation_speech({
                        let state = Arc::clone(&sink.state);
                        let instance_id = sink.instance_id.clone();
                        let binding_id = sink.binding_id.clone();
                        let agent_id = sink.agent_id.clone();
                        let session_id = sink.session_id.clone();
                        let reply_target = reply_target.clone();
                        let latest = Arc::clone(&last_continuation_say);
                        let delivery_mode = sink.delivery_mode;
                        Arc::new(move |speech: String| {
                            let state = Arc::clone(&state);
                            let instance_id = instance_id.clone();
                            let binding_id = binding_id.clone();
                            let agent_id = agent_id.clone();
                            let session_id = session_id.clone();
                            let reply_target = reply_target.clone();
                            let latest = Arc::clone(&latest);
                            Box::pin(async move {
                                if delivery_mode == DeliveryMode::Say {
                                    let delivery_id = crate::delivery::deliver_intermediate_say(
                                        &state,
                                        &instance_id,
                                        &binding_id,
                                        &agent_id,
                                        &session_id,
                                        &speech,
                                        reply_target.as_deref(),
                                    )
                                    .await
                                    .map_err(|e| {
                                        anyhow::anyhow!(
                                            "extgate resume intermediate say failed: {}",
                                            e.code.as_str()
                                        )
                                    })?;
                                    *latest.lock().expect("continuation say id lock") =
                                        Some(delivery_id);
                                }
                                Ok(())
                            })
                        })
                    });
                    // 連鎖: この resume が更に subtask を spawn したら、その完了 say も
                    // 同じ発端 origin へ返せるよう reply_target を引き継ぐ。
                    if let Some(rt) = reply_target.clone() {
                        req = req.with_reply_target(rt);
                    }
                    if let Some(ga) = ops_actions.clone() {
                        req = req.with_gateway_actions(ga);
                    }
                    v3_attach_dispatch(
                        req,
                        &kind_id,
                        author_id.clone(),
                        registry.clone(),
                        Arc::clone(&dispatch),
                    )
                },
            )
            .await;
            let engine_completion = turn.as_ref().and_then(|r| {
                r.as_ref().ok().map(|er| {
                    (
                        er.last_posting_utterance_id.clone(),
                        er.stopped_by_limit,
                        er.last_generation_had_continuation_speech,
                    )
                })
            });
            let effect = match turn {
                Some(r) => delivery_effect(
                    r,
                    opencrab_actions::DeliveryContext {
                        session_id: &sink.session_id,
                        agent_id: &sink.agent_id,
                        origin: "extgate",
                    },
                ),
                None => opencrab_actions::DeliveryEffect::Empty,
            };
            let effect = adjust_inbound_effect(sink.delivery_mode, effect);
            let final_say_id = apply_delivery_effect(
                &sink.state,
                &sink.instance_id,
                &sink.binding_id,
                &sink.agent_id,
                &sink.session_id,
                effect,
                reply_target.as_deref(),
            )
            .await;
            let started_subtask = subtask_starts.load(std::sync::atomic::Ordering::SeqCst) > 0;
            // §13.3.1 案E: 進行中判定は agent 単位（別 session の未決着 subtask も含む）。
            let agent_has_running = sink.runtime.has_running_subtask_for_agent(&sink.agent_id);
            let completed_target = select_completed_target(
                engine_completion,
                started_subtask,
                agent_has_running,
                final_say_id,
                last_continuation_say
                    .lock()
                    .expect("continuation say id lock")
                    .clone(),
            );
            emit_activity(
                &sink.state,
                &sink.instance_id,
                &sink.binding_id,
                &activity_id,
                "ended",
                None,
                completed_target.as_deref(),
            )
            .await;
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::session_id_for_binding;
    use opencrab_actions::{CallerIdentity, NoopCompletionSink, SubtaskRegistries};
    use std::sync::Arc;

    #[test]
    fn v3_run_request_always_attaches_dispatch() {
        let session_id = session_id_for_binding("bind-1");
        let registry = SubtaskRegistries::new().registry_for(&session_id);
        let req = v3_attach_dispatch(
            RunRequest::new(
                "agent-1",
                "A",
                session_id,
                "sys",
                "conv",
                "extgate",
                CallerIdentity::Owner,
            ),
            "web",
            "author",
            registry,
            Arc::new(NoopCompletionSink),
        );
        assert!(
            req.completion_sink.is_some(),
            "V3 は常に with_dispatch する（sink 無しだと process.rs が dispatcher を刺さない）"
        );
        assert!(req.subtask_registry.is_some());
        assert_eq!(req.gateway, "extgate");
        assert!(req.session_id.starts_with(EXTGATE_SESSION_PREFIX));
    }
}
