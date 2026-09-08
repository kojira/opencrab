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

#[async_trait]
pub trait V3ProcessControl: Send + Sync {
    async fn start(&self, agent_id: &str) -> anyhow::Result<()>;
    async fn stop(&self, agent_id: &str);
    async fn shutdown_all(&self);
}

struct V3IdentityProvisioning {
    inner: Arc<dyn GatewayIdentityProvisioning>,
    process: Arc<dyn V3ProcessControl>,
}

#[async_trait]
impl GatewayIdentityProvisioning for V3IdentityProvisioning {
    async fn adopt_identity(&self, agent_id: &str, identity: &str) -> anyhow::Result<String> {
        let adopted = self.inner.adopt_identity(agent_id, identity).await?;
        self.process.start(agent_id).await?;
        Ok(adopted)
    }
}

/// V3-only transport decorator. Lifecycle/capabilities remain on the core manager, but runtime
/// liveness is true only after the external gateway has registered with extgate.
pub struct V3OnlyGateway {
    inner: SharedAgentGateway,
    v3_live: V3LivenessProbe,
    process: Option<Arc<dyn V3ProcessControl>>,
}

impl V3OnlyGateway {
    pub fn new(inner: SharedAgentGateway, v3_live: V3LivenessProbe) -> Self {
        Self {
            inner,
            v3_live,
            process: None,
        }
    }

    pub fn with_process(mut self, process: Arc<dyn V3ProcessControl>) -> Arc<Self> {
        self.process = Some(process);
        Arc::new(self)
    }
}

#[async_trait]
impl AgentGatewayLifecycle for V3OnlyGateway {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        self.inner.start(agent_id).await?;
        if let Some(process) = &self.process {
            process.start(agent_id).await?;
        }
        Ok(())
    }

    async fn stop(&self, agent_id: &str) {
        if let Some(process) = &self.process {
            process.stop(agent_id).await;
        }
        self.inner.stop(agent_id).await
    }

    fn is_running(&self, agent_id: &str) -> bool {
        (self.v3_live)(agent_id)
    }

    async fn restore_all(&self) {
        self.inner.restore_all().await
    }

    async fn shutdown_all(&self) {
        if let Some(process) = &self.process {
            process.shutdown_all().await;
        }
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
        let inner = self.inner.identity_provisioning()?;
        match &self.process {
            Some(process) => Some(Arc::new(V3IdentityProvisioning {
                inner,
                process: process.clone(),
            })),
            None => Some(inner),
        }
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
        let gateway = Arc::new(V3OnlyGateway::new(inner, probe));
        assert!(!gateway.is_running("core-only"));
        assert!(gateway.is_running("externally-live"));
        assert!(gateway.gateway_actions_for("core-only").is_none());
    }
}
