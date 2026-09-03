use std::sync::Arc;
use std::time::Duration;

use super::hello::spawn_bind_timeout;
use crate::close::close_live;
use crate::error::ErrorCode;
use crate::protocol::{bind_frame, write_json};
use crate::registry::{ExtgateState, Pending};

/// 当該 binding が acknowledged になるまで待つ。live 消失は即 false（理由を残す）。
pub async fn wait_bind_ack(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let Ok(reg) = state.lock_registry() else {
                tracing::warn!(
                    instance_id,
                    binding_id,
                    "wait_bind_ack: registry lock failed"
                );
                return false;
            };
            match reg.get(instance_id) {
                Some(live) if live.acknowledged.contains(binding_id) => return true,
                Some(_) => {}
                None => {
                    tracing::warn!(
                        instance_id,
                        binding_id,
                        "wait_bind_ack: live connection gone"
                    );
                    return false;
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!(instance_id, binding_id, "wait_bind_ack: timeout");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// open binding と registry から Web 投影の state を導出する。
/// pending 待ちだけが provisioning。live 不在・enqueue 未実行は unavailable。
pub fn web_binding_state(
    reg: &crate::registry::Registry,
    instance_id: &str,
    binding_id: &str,
) -> &'static str {
    match reg.get(instance_id) {
        Some(live) if live.acknowledged.contains(binding_id) => "ready",
        Some(live)
            if live
                .pending
                .values()
                .any(|p| p.binding_id() == Some(binding_id)) =>
        {
            "provisioning"
        }
        Some(_) | None => "unavailable",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueBindOutcome {
    Written,
    AlreadyPending,
    AlreadyAcknowledged,
    NotLive,
    RegistryLockFailed,
    WriteFailed,
}

impl EnqueueBindOutcome {
    pub fn started_wait(self) -> bool {
        matches!(
            self,
            Self::Written | Self::AlreadyPending | Self::AlreadyAcknowledged
        )
    }
}

/// 新規 Binding PUT 後の bind exact 1。lock 失敗・対象不在は warn（黙って return しない）。
pub async fn enqueue_bind(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    address: &str,
) -> EnqueueBindOutcome {
    let (writer, identity) = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!(
                    instance_id,
                    binding_id,
                    "enqueue_bind: registry lock failed"
                );
                return EnqueueBindOutcome::RegistryLockFailed;
            }
        };
        let Some(live) = reg.get_mut(instance_id) else {
            tracing::warn!(instance_id, binding_id, "enqueue_bind: instance not live");
            return EnqueueBindOutcome::NotLive;
        };
        let req_id = crate::ids::bind_request_id(binding_id);
        if live.pending.contains_key(&req_id) {
            return EnqueueBindOutcome::AlreadyPending;
        }
        if live.acknowledged.contains(binding_id) {
            return EnqueueBindOutcome::AlreadyAcknowledged;
        }
        live.pending.insert(
            req_id,
            Pending::Bind {
                binding_id: binding_id.to_string(),
                started: std::time::Instant::now(),
            },
        );
        (live.writer.clone(), live.identity)
    };
    crate::race::park("after_pending").await;
    if write_json(&writer, &bind_frame(binding_id, address))
        .await
        .is_err()
    {
        tracing::warn!(instance_id, binding_id, "enqueue_bind: bind write failed");
        close_live(
            state,
            Some(instance_id),
            Some(identity),
            ErrorCode::BindFailed,
            None,
            None,
        )
        .await;
        return EnqueueBindOutcome::WriteFailed;
    }
    spawn_bind_timeout(
        Arc::clone(state),
        instance_id.to_string(),
        binding_id.to_string(),
        identity,
    );
    EnqueueBindOutcome::Written
}
