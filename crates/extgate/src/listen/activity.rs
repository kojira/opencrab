use std::sync::Arc;

use crate::protocol::{activity_frame, turn_failed_frame, write_json};
use crate::registry::ExtgateState;

pub async fn emit_activity(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    activity_id: &str,
    activity_state: &str,
    // R2(👀): started が読み取るターン発端の origin。started のときだけ Some を渡す（ended は None）。
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
    let _ = write_json(
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
