//! V3-only gateway liveness decorator.
//!
//! Runtime liveness comes only from the external gateway registry, never from a core keepalive.

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_actions::{
    AgentGatewayLifecycle, GatewayIdentityProvisioning, GatewayKeyProvisioning,
    GatewayNostrPassthrough, SharedAgentGateway,
};

/// V3 gateway の liveness を返す probe（agent_id → 稼働中か）。
pub type V3LivenessProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// V3-only transport decorator. Lifecycle/capabilities remain on the core manager, but runtime
/// liveness is true only after the external gateway has registered with extgate.
pub struct V3OnlyGateway {
    inner: SharedAgentGateway,
    v3_live: V3LivenessProbe,
}

impl V3OnlyGateway {
    pub fn new(inner: SharedAgentGateway, v3_live: V3LivenessProbe) -> Arc<Self> {
        Arc::new(Self { inner, v3_live })
    }
}

#[async_trait]
impl AgentGatewayLifecycle for V3OnlyGateway {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        self.inner.start(agent_id).await
    }

    async fn stop(&self, agent_id: &str) {
        self.inner.stop(agent_id).await
    }

    fn is_running(&self, agent_id: &str) -> bool {
        (self.v3_live)(agent_id)
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
        if self.is_running(agent_id) {
            self.inner.gateway_actions_for(agent_id)
        } else {
            None
        }
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
    async fn v3_only_never_reports_core_keep_alive_as_external_liveness() {
        let inner = Arc::new(FakeInner {
            running: "core-only",
            started: AtomicBool::new(false),
        });
        let probe: V3LivenessProbe = Arc::new(|agent_id: &str| agent_id == "externally-live");
        let gateway = V3OnlyGateway::new(inner, probe);
        assert!(!gateway.is_running("core-only"));
        assert!(gateway.is_running("externally-live"));
        assert!(gateway.gateway_actions_for("core-only").is_none());
    }
}
