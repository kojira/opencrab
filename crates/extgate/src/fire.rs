//! Generic session timed-turn sink.

use std::sync::Arc;

use opencrab_actions::{AgentRuntime, TimedFireRequest, TimedFireSink};

use crate::completion::{run_v3_said_less_turn, ExtgateCompletionSink};
use crate::registry::ExtgateState;

/// D-1099: the session's gateway record, if its instance is live. The gateway owns the session
/// key; core only needs the instance to be connected.
fn resolve_live_binding(
    state: &ExtgateState,
    binding_id: &str,
    session_id: &str,
    expected_agent_id: &str,
) -> Option<opencrab_db::queries::GateSession> {
    let found = {
        let conn = state.db.lock().ok()?;
        opencrab_db::queries::gate_session_by_row(&conn, binding_id).ok()??
    };
    if found.session_id != session_id || found.agent_id != expected_agent_id {
        return None;
    }
    state.registry.lock().ok()?.is_live(&found.instance_id).then_some(found)
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
            model_profile: opencrab_actions::TurnModelProfile::Default,
            activity: None,
        };
        tokio::spawn(async move {
            run_v3_said_less_turn(sink, req.caller).await;
        });
    }
}
