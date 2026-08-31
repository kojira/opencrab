//! 専用 gateway の liveness に V3 gateway を OR する透過デコレータ（DESIGN-DISCORD-GATE §8.1）。
//!
//! 併存期（legacy per-agent gateway + 新 V3 gateway process）に、共有 `message_loop` の
//! `served_by_dedicated_gateway`（= 登録簿の `is_running`）が **どちらか一方でも稼働中**なら
//! 対象 agent を除外するようにする。これが二重受信防止 lever であり、落とすと同一 channel で
//! 新旧が二重応答する。
//!
//! V3 の liveness は core の in-memory live registry（`ExtgateState::agent_has_live_gateway`）が
//! 正で、DB の enabled フラグではない（#40 の教訓: enabled=1 でも接続が死んでいれば false へ倒し、
//! どの gateway からも応答しない状態を作らない）。
//!
//! `is_running` 以外は inner（legacy manager）へ委譲する純粋なデコレータ。

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_actions::{
    AgentGatewayLifecycle, GatewayIdentityProvisioning, GatewayKeyProvisioning,
    GatewayNostrPassthrough, SharedAgentGateway,
};

/// V3 gateway の liveness を返す probe（agent_id → 稼働中か）。
pub type V3LivenessProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// legacy gateway を包み、`is_running` に V3 liveness を OR する。他メソッドは inner へ委譲。
pub struct V3AwareGateway {
    inner: SharedAgentGateway,
    v3_live: V3LivenessProbe,
}

impl V3AwareGateway {
    pub fn new(inner: SharedAgentGateway, v3_live: V3LivenessProbe) -> Arc<Self> {
        Arc::new(Self { inner, v3_live })
    }
}

#[async_trait]
impl AgentGatewayLifecycle for V3AwareGateway {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        self.inner.start(agent_id).await
    }

    async fn stop(&self, agent_id: &str) {
        self.inner.stop(agent_id).await
    }

    /// legacy が稼働中、または V3 gateway が当該 agent を受信できる状態なら true。
    fn is_running(&self, agent_id: &str) -> bool {
        self.inner.is_running(agent_id) || (self.v3_live)(agent_id)
    }

    async fn restore_all(&self) {
        self.inner.restore_all().await
    }

    async fn shutdown_all(&self) {
        self.inner.shutdown_all().await
    }

    fn gateway_actions_for(
        &self,
        agent_id: &str,
    ) -> Option<Arc<dyn opencrab_gateway::GatewayActions>> {
        self.inner.gateway_actions_for(agent_id)
    }

    fn key_provisioning(&self) -> Option<Arc<dyn GatewayKeyProvisioning>> {
        self.inner.key_provisioning()
    }

    fn identity_provisioning(&self) -> Option<Arc<dyn GatewayIdentityProvisioning>> {
        self.inner.identity_provisioning()
    }

    fn nostr_passthrough(&self) -> Option<Arc<dyn GatewayNostrPassthrough>> {
        self.inner.nostr_passthrough()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FakeInner {
        running: &'static str,
        started: AtomicBool,
    }

    #[async_trait]
    impl AgentGatewayLifecycle for FakeInner {
        fn kind(&self) -> &'static str {
            "discord"
        }
        async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self, _agent_id: &str) {}
        fn is_running(&self, agent_id: &str) -> bool {
            agent_id == self.running
        }
        async fn restore_all(&self) {}
        async fn shutdown_all(&self) {}
    }

    #[tokio::test]
    async fn is_running_ors_legacy_and_v3() {
        let inner = Arc::new(FakeInner {
            running: "legacy-agent",
            started: AtomicBool::new(false),
        });
        let probe: V3LivenessProbe = Arc::new(|agent_id: &str| agent_id == "v3-agent");
        let deco = V3AwareGateway::new(inner.clone(), probe);

        // legacy 側で稼働 → true。
        assert!(deco.is_running("legacy-agent"));
        // V3 側で稼働 → true（legacy は false でも OR で拾う）。
        assert!(deco.is_running("v3-agent"));
        // どちらも非稼働 → false（共有側が処理を続ける）。
        assert!(!deco.is_running("nobody"));

        // 他メソッドは inner へ委譲。
        assert_eq!(deco.kind(), "discord");
        deco.start("x").await.unwrap();
        assert!(
            inner.started.load(Ordering::SeqCst),
            "start が inner へ委譲される"
        );
    }

    #[tokio::test]
    async fn is_running_true_when_only_v3_live() {
        // legacy がどの agent でも非稼働（起動失敗相当）でも、V3 が生きていれば除外される。
        let inner = Arc::new(FakeInner {
            running: "",
            started: AtomicBool::new(false),
        });
        let probe: V3LivenessProbe = Arc::new(|agent_id: &str| agent_id == "crab");
        let deco = V3AwareGateway::new(inner, probe);
        assert!(deco.is_running("crab"));
        assert!(!deco.is_running("other"));
    }
}
