use std::sync::Arc;

use crate::protocol::{activity_frame, ended_activity_frame, turn_failed_frame, write_json};
use crate::registry::ExtgateState;

/// `binding_id` is core's internal record of the session; the frame carries the gateway's key.
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
    let Some((writer, session_key)) = state.live_route(instance_id, binding_id) else {
        return;
    };
    let _ = write_json(
        &writer,
        &activity_frame(
            &session_key,
            activity_id,
            activity_state,
            origin,
            completed_target,
        ),
    )
    .await;
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
    let Some((writer, session_key)) = state.live_route(instance_id, binding_id) else {
        return;
    };
    let _ = write_json(
        &writer,
        &ended_activity_frame(&session_key, activity_id, completed_target, silent_origins),
    )
    .await;
}

/// R3(❌): ターン失敗（DeliveryEffect::Failed）を発端 origin つきで gateway へ通知する。
/// 応答は返らない。
pub async fn emit_turn_failed(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    origin: &str,
) {
    let Some((writer, session_key)) = state.live_route(instance_id, binding_id) else {
        return;
    };
    let _ = write_json(&writer, &turn_failed_frame(&session_key, origin)).await;
}
