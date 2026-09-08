use anyhow::Context as _;
use opencrab_server::dedicated_gateway::V3ProcessControl;
use opencrab_server::discord_supervisor::{
    GatewayChildSpawner, GatewaySupervisorSet, SupervisorConfig,
};

pub(super) struct NostrV3Controller {
    db: opencrab_db::Db,
    placement_dir: std::path::PathBuf,
    core_socket: String,
    secret_provider: opencrab_nostr::MainKeyProvider,
    gateway_bin: std::path::PathBuf,
    nostaro_bin: std::path::PathBuf,
    supervisors: std::sync::Arc<GatewaySupervisorSet>,
}

impl NostrV3Controller {
    pub(super) fn new(
        db: &opencrab_db::Db,
        database_path: &str,
        core_socket: &str,
        secret_provider: &opencrab_nostr::MainKeyProvider,
        gateway_bin: &std::path::Path,
        nostaro_bin: &std::path::Path,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        let placement_dir = std::path::Path::new(database_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("gate")
            .join("nostr");
        std::fs::create_dir_all(&placement_dir)
            .with_context(|| format!("placement dir 作成失敗: {}", placement_dir.display()))?;
        Ok(std::sync::Arc::new(Self {
            db: db.clone(),
            placement_dir,
            core_socket: core_socket.to_string(),
            secret_provider: secret_provider.clone(),
            gateway_bin: gateway_bin.to_path_buf(),
            nostaro_bin: nostaro_bin.to_path_buf(),
            supervisors: GatewaySupervisorSet::new(SupervisorConfig::default()),
        }))
    }

    fn plan(
        &self,
        agent_id: &str,
    ) -> anyhow::Result<opencrab_server::nostr_provision::NostrPlacementPlan> {
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for Nostr V3 ignition"))?;
        opencrab_server::nostr_provision::load_nostr_placement_plans(&conn)?
            .into_iter()
            .find(|plan| plan.agent_id == agent_id)
            .with_context(|| format!("enabled Nostr V3 placement not found for {agent_id}"))
    }

    async fn start_plan(
        &self,
        plan: opencrab_server::nostr_provision::NostrPlacementPlan,
    ) -> anyhow::Result<()> {
        let secret = (self.secret_provider)(&plan.agent_id)?;
        let placement = serde_json::json!({
            "core_socket": self.core_socket,
            "nostaro_bin": self.nostaro_bin,
            "instances": [{
                "instance_id": plan.instance_id,
                "revision": plan.revision,
                "address": plan.address,
                "config_b64": plan.config_b64,
            }],
        });
        let path = self.placement_dir.join(format!("{}.json", plan.agent_id));
        std::fs::write(&path, serde_json::to_vec_pretty(&placement)?)
            .with_context(|| format!("placement 書き出し失敗: {}", path.display()))?;
        let spawner = std::sync::Arc::new(GatewayChildSpawner::new_nostr(
            self.gateway_bin.clone(),
            path,
            secret.to_string(),
            plan.agent_id.clone(),
        ));
        self.supervisors.start(&plan.agent_id, spawner).await;
        tracing::info!(
            agent_id = %plan.agent_id,
            bin = %self.gateway_bin.display(),
            "nostr-gateway supervisor started; secret injected only in child env"
        );
        Ok(())
    }

    pub(super) async fn start_all(&self) -> anyhow::Result<()> {
        let plans = {
            let conn = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("db lock for Nostr V3 ignition"))?;
            opencrab_server::nostr_provision::load_nostr_placement_plans(&conn)?
        };
        for plan in plans {
            self.start_plan(plan).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl V3ProcessControl for NostrV3Controller {
    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        self.start_plan(self.plan(agent_id)?).await
    }

    async fn stop(&self, agent_id: &str) {
        self.supervisors.stop(agent_id).await;
    }

    async fn shutdown_all(&self) {
        self.supervisors.shutdown_all().await;
    }
}
