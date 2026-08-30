//! V3 turn の subtask 決着 sink。既存の `with_dispatch` / `run_session_turn` を再利用する。
//!
//! Discord / Nostr / REST は `RunRequest::with_dispatch` を付ける。V3（extgate inbound）
//! だけが付けていなかったため、`process.rs` が dispatcher を刺さず SkillEngine が
//! 全ツールを `execute_with_id` で同期実行していた。

use std::sync::Arc;

use opencrab_actions::{
    delivery_effect, run_session_turn, AgentRuntime, LiveInboundScope, RunRequest,
    SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled,
};

use crate::delivery::apply_delivery_effect;
use crate::delivery_mode::{adjust_inbound_effect, DeliveryMode};
use crate::registry::ExtgateState;

/// `session_id_for_binding` と同じ接頭辞。`dispatch_settled` が親セッション判定に使う。
pub const EXTGATE_SESSION_PREFIX: &str = "extgate-";

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
    let locks = sink.runtime.session_locks();
    let session_id = sink.session_id.clone();
    locks
        .run_serialized(&session_id, async move {
            let caller = ev.caller.clone();
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
            let turn = run_session_turn(
                &sink.runtime,
                &sink.session_id,
                &sink.agent_id,
                &system,
                "",
                |raw| raw.to_string(),
                |conversation| {
                    v3_attach_dispatch(
                        RunRequest::new(
                            sink.agent_id.clone(),
                            name.clone(),
                            sink.session_id.clone(),
                            system.clone(),
                            conversation,
                            "extgate",
                            caller.clone(),
                        ),
                        &kind_id,
                        author_id.clone(),
                        registry.clone(),
                        Arc::clone(&dispatch),
                    )
                },
            )
            .await;
            let effect = match turn {
                Some(r) => delivery_effect(r),
                None => opencrab_actions::DeliveryEffect::Empty,
            };
            let effect = adjust_inbound_effect(sink.delivery_mode, effect);
            apply_delivery_effect(
                &sink.state,
                &sink.runtime,
                &sink.instance_id,
                &sink.binding_id,
                &sink.agent_id,
                &sink.session_id,
                effect,
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
