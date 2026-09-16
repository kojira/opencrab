use std::sync::Arc;

use crate::protocol::{activity_frame, ended_activity_frame, turn_failed_frame, write_json};
use crate::registry::ExtgateState;

pub fn llm_activity_hook(
    state: Arc<ExtgateState>,
    instance_id: String,
    binding_id: String,
    activity_id: String,
    activity_state: &'static str,
) -> opencrab_actions::LlmActivityHook {
    Arc::new(move || {
        let state = Arc::clone(&state);
        let instance_id = instance_id.clone();
        let binding_id = binding_id.clone();
        let activity_id = activity_id.clone();
        Box::pin(async move {
            emit_activity(
                &state,
                &instance_id,
                &binding_id,
                &activity_id,
                activity_state,
                None,
                None,
            )
            .await;
        })
    })
}

pub async fn emit_activity(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    activity_id: &str,
    activity_state: &str,
    // #964: LLM request 直前の read 通知だけが origin を持つ。started / ended は None。
    origin: Option<&str>,
    // #915: ended で 🏁 を付ける say delivery_id / reply call_id。無ければ field を送らない。
    completed_target: Option<&str>,
) {
    let writer = {
        let Ok(reg) = state.lock_registry() else {
            return;
        };
        let Some(live) = reg.get(instance_id) else {
            return;
        };
        if !live.acknowledged.contains(binding_id) {
            return;
        }
        live.writer.clone()
    };
    let log_llm_boundary = matches!(activity_state, "started" | "stopped");
    if log_llm_boundary {
        tracing::info!(
            event = "activity_emit_started",
            activity_id,
            state = activity_state,
            "activity emit starting"
        );
    }
    let write_result = write_json(
        &writer,
        &activity_frame(
            binding_id,
            activity_id,
            activity_state,
            origin,
            completed_target,
        ),
    )
    .await;
    if log_llm_boundary {
        tracing::info!(
            event = "activity_emit_completed",
            activity_id,
            state = activity_state,
            outcome = if write_result.is_ok() {
                "success"
            } else {
                "error"
            },
            "activity emit completed"
        );
    }
}

/// Successful executionのauthoritative ended outcome。new coreは沈黙originが無くても
/// `silent_origins: []`を必ず送る。
pub async fn emit_ended_activity(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    activity_id: &str,
    completed_target: Option<&str>,
    silent_origins: &[String],
) {
    let writer = {
        let Ok(reg) = state.lock_registry() else {
            return;
        };
        let Some(live) = reg.get(instance_id) else {
            return;
        };
        if !live.acknowledged.contains(binding_id) {
            return;
        }
        live.writer.clone()
    };
    tracing::info!(
        event = "turn_final_activity_ended_emit_started",
        activity_id,
        state = "ended",
        "turn-final activity ended emit starting"
    );
    let write_result = write_json(
        &writer,
        &ended_activity_frame(binding_id, activity_id, completed_target, silent_origins),
    )
    .await;
    tracing::info!(
        event = "turn_final_activity_ended_emit_completed",
        activity_id,
        state = "ended",
        outcome = if write_result.is_ok() {
            "success"
        } else {
            "error"
        },
        "turn-final activity ended emit completed"
    );
}

/// R3(❌): ターン失敗（DeliveryEffect::Failed）を発端 origin つきで gateway へ通知する。
/// emit_activity と同じ writer 解決経路（未 ack binding は write 0）。応答は返らない。
pub async fn emit_turn_failed(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    origin: &str,
) {
    let writer = {
        let Ok(reg) = state.lock_registry() else {
            return;
        };
        let Some(live) = reg.get(instance_id) else {
            return;
        };
        if !live.acknowledged.contains(binding_id) {
            return;
        }
        live.writer.clone()
    };
    let _ = write_json(&writer, &turn_failed_frame(binding_id, origin)).await;
}
