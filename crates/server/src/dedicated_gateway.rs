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
    fn validate_start(&self) -> anyhow::Result<()>;
    async fn start(&self, agent_id: &str) -> anyhow::Result<()>;
    async fn stop(&self, agent_id: &str);
    async fn shutdown_all(&self);
}

struct V3IdentityProvisioning {
    identity: Arc<dyn GatewayIdentityProvisioning>,
    gateway: SharedAgentGateway,
    process: Arc<dyn V3ProcessControl>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
}

#[async_trait]
impl GatewayIdentityProvisioning for V3IdentityProvisioning {
    async fn adopt_identity(&self, agent_id: &str, identity: &str) -> anyhow::Result<String> {
        let _lifecycle = self.lifecycle.lock().await;
        self.process.stop(agent_id).await;
        self.process.validate_start()?;
        let adopted = self.identity.adopt_identity(agent_id, identity).await?;
        if let Err(error) = self.process.start(agent_id).await {
            self.gateway.stop(agent_id).await;
            return Err(error);
        }
        Ok(adopted)
    }
}

/// V3-only transport decorator. Lifecycle/capabilities remain on the core manager, but runtime
/// liveness is true only after the external gateway has registered with extgate.
pub struct V3OnlyGateway {
    inner: SharedAgentGateway,
    v3_live: V3LivenessProbe,
    process: Option<Arc<dyn V3ProcessControl>>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
}

impl V3OnlyGateway {
    pub fn new(inner: SharedAgentGateway, v3_live: V3LivenessProbe) -> Self {
        Self {
            inner,
            v3_live,
            process: None,
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
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
        let _lifecycle = self.lifecycle.lock().await;
        // Restart is stop-first. If core provisioning fails, no stale external child may keep
        // serving the previous placement/credential.
        if let Some(process) = &self.process {
            process.stop(agent_id).await;
            process.validate_start()?;
        }
        self.inner.start(agent_id).await?;
        if let Some(process) = &self.process {
            if let Err(error) = process.start(agent_id).await {
                self.inner.stop(agent_id).await;
                return Err(error);
            }
        }
        Ok(())
    }

    async fn stop(&self, agent_id: &str) {
        let _lifecycle = self.lifecycle.lock().await;
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
        let _lifecycle = self.lifecycle.lock().await;
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
                identity: inner,
                gateway: self.inner.clone(),
                process: process.clone(),
                lifecycle: self.lifecycle.clone(),
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

    struct RejectingProcess;

    #[async_trait]
    impl V3ProcessControl for RejectingProcess {
        fn validate_start(&self) -> anyhow::Result<()> {
            anyhow::bail!("V3 prerequisites unavailable")
        }
        async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
            panic!("start must not run after validation failure")
        }
        async fn stop(&self, _agent_id: &str) {}
        async fn shutdown_all(&self) {}
    }

    #[tokio::test]
    async fn v3_prerequisites_are_checked_before_inner_runtime_side_effects() {
        let inner = Arc::new(FakeInner {
            running: "none",
            started: AtomicBool::new(false),
        });
        let gateway = V3OnlyGateway::new(inner.clone(), Arc::new(|_| false))
            .with_process(Arc::new(RejectingProcess));
        let error = gateway.start("agent").await.unwrap_err();
        assert!(error.to_string().contains("prerequisites"));
        assert!(!inner.started.load(Ordering::SeqCst));
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
