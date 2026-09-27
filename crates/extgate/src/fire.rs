//! Generic binding/session timed-turn sink.

use std::sync::Arc;

use opencrab_actions::{AgentRuntime, TimedFireRequest, TimedFireSink};

use crate::completion::{run_v3_said_less_turn, ExtgateCompletionSink};
use crate::inbound::{resolve_binding_context, BindingContext};
use crate::registry::ExtgateState;

fn resolve_live_binding(
    state: &ExtgateState,
    binding_id: &str,
    session_id: &str,
    expected_agent_id: &str,
) -> Option<BindingContext> {
    let ctx = {
        let conn = state.db.lock().ok()?;
        let (stored_session_id, closed_at): (String, Option<i64>) = conn
            .query_row(
                "SELECT session_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
                [binding_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()?;
        if closed_at.is_some() || stored_session_id != session_id {
            return None;
        }
        let ctx = resolve_binding_context(&conn, binding_id)?;
        if ctx.agent_id != expected_agent_id {
            return None;
        }
        ctx
    };
    let registry = state.registry.lock().ok()?;
    let live = registry.get(&ctx.instance_id)?;
    if !live.acknowledged.contains(binding_id) {
        return None;
    }
    Some(ctx)
}

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
        let ctx = match resolve_live_binding(
            &self.state,
            &req.binding_id,
            &req.session_id,
            &req.agent_id,
        ) {
            Some(context) => context,
            None => {
                tracing::warn!(
                    binding_id = req.binding_id,
                    session_id = req.session_id,
                    agent_id = req.agent_id,
                    "timed-fire: live generic binding could not be resolved"
                );
                return;
            }
        };
        let sink = ExtgateCompletionSink {
            state: Arc::clone(&self.state),
            runtime: self.runtime.clone(),
            instance_id: ctx.instance_id,
            binding_id: req.binding_id,
            agent_id: ctx.agent_id,
            session_id: req.session_id,
            only_speaker: false,
            speaker_id: String::new(),
            system_context: req.prompt,
        };
        tokio::spawn(async move {
            run_v3_said_less_turn(sink, req.caller, None).await;
        });
    }
}
