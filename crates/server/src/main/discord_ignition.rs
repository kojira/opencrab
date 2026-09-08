use anyhow::Context as _;
use opencrab_actions::AgentGatewayLifecycle;
use opencrab_server::discord_provision::{load_discord_launch_plan, DiscordLaunchPlan};
use opencrab_server::discord_supervisor::{
    GatewayChildSpawner, GatewaySupervisorSet, SupervisorConfig,
};

pub(super) struct DiscordV3Controller {
    db: opencrab_db::Db,
    extgate: std::sync::Arc<opencrab_extgate::ExtgateState>,
    placement_dir: std::path::PathBuf,
    core_socket: String,
    attachment_spool_root: std::path::PathBuf,
    gateway_bin: std::path::PathBuf,
    supervisors: std::sync::Arc<GatewaySupervisorSet>,
}

impl DiscordV3Controller {
    pub(super) fn new(
        db: &opencrab_db::Db,
        extgate: std::sync::Arc<opencrab_extgate::ExtgateState>,
        database_path: &str,
        core_socket: &str,
        attachment_spool_root: &std::path::Path,
        gateway_bin: &std::path::Path,
    ) -> anyhow::Result<std::sync::Arc<Self>> {
        let placement_dir = std::path::Path::new(database_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("gate")
            .join("discord");
        std::fs::create_dir_all(&placement_dir)
            .with_context(|| format!("placement dir 作成失敗: {}", placement_dir.display()))?;
        Ok(std::sync::Arc::new(Self {
            db: db.clone(),
            extgate,
            placement_dir,
            core_socket: core_socket.to_string(),
            attachment_spool_root: attachment_spool_root.to_path_buf(),
            gateway_bin: gateway_bin.to_path_buf(),
            supervisors: GatewaySupervisorSet::new(SupervisorConfig::default()),
        }))
    }

    fn plan(&self, agent_id: &str) -> anyhow::Result<DiscordLaunchPlan> {
        let mut conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for Discord V3 ignition"))?;
        load_discord_launch_plan(&mut conn, agent_id, opencrab_extgate::now_nanos())
    }

    async fn start_plan(&self, launch: DiscordLaunchPlan) -> anyhow::Result<()> {
        let DiscordLaunchPlan {
            placement: plan,
            bot_token,
        } = launch;
        let placement = serde_json::json!({
            "core_socket": self.core_socket,
            "attachment_spool_root": self.attachment_spool_root,
            "instances": [{
                "instance_id": plan.instance_id,
                "revision": plan.revision,
                "addresses": plan.addresses,
                "config_b64": plan.config_b64,
            }],
        });
        let path = self.placement_dir.join(format!("{}.json", plan.agent_id));
        std::fs::write(&path, serde_json::to_vec_pretty(&placement)?)
            .with_context(|| format!("placement 書き出し失敗: {}", path.display()))?;
        let spawner = std::sync::Arc::new(GatewayChildSpawner::new(
            self.gateway_bin.clone(),
            path,
            bot_token,
            plan.agent_id.clone(),
        ));
        self.supervisors.start(&plan.agent_id, spawner).await;
        tracing::info!(
            agent_id = %plan.agent_id,
            bin = %self.gateway_bin.display(),
            "discord-gateway supervisor started; token injected only in child env"
        );
        Ok(())
    }

    async fn ensure_bot_user_id(&self, agent_id: &str, token: &str) -> anyhow::Result<()> {
        // The token identifies the bot. Revalidate on every process start so token rotation
        // cannot reuse a stale self_bot_id and ingest the new bot's own messages.
        let response = reqwest::Client::new()
            .get("https://discord.com/api/v10/users/@me")
            .header(reqwest::header::AUTHORIZATION, format!("Bot {token}"))
            .send()
            .await
            .context("Discord bot identity request failed")?
            .error_for_status()
            .context("Discord bot identity rejected")?;
        let body: serde_json::Value = response
            .json()
            .await
            .context("Discord bot identity response was invalid")?;
        let bot_user_id = body
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .context("Discord bot identity response had no id")?;
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for Discord identity update"))?;
        opencrab_db::queries::set_agent_discord_bot_user_id(&conn, agent_id, bot_user_id)?;
        Ok(())
    }

    pub(super) async fn start_all(&self) -> anyhow::Result<()> {
        let ids: Vec<String> = {
            let conn = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("db lock for Discord V3 ignition"))?;
            opencrab_db::queries::list_enabled_agent_discord_configs(&conn)?
                .into_iter()
                .map(|config| config.agent_id)
                .collect()
        };
        for agent_id in ids {
            AgentGatewayLifecycle::start(self, &agent_id).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AgentGatewayLifecycle for DiscordV3Controller {
    fn kind(&self) -> &'static str {
        opencrab_actions::gateway_kinds::DISCORD
    }

    async fn start(&self, agent_id: &str) -> anyhow::Result<()> {
        let config = {
            let conn = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("db lock for Discord V3 start"))?;
            opencrab_db::queries::get_agent_discord_config(&conn, agent_id)?
                .with_context(|| format!("Discord config not found for {agent_id}"))?
        };
        if !config.enabled {
            return Err(opencrab_actions::StartDeclined::err(
                self.kind(),
                agent_id,
                "設定が無効です",
            ));
        }
        if config.bot_token.trim().is_empty() {
            return Err(opencrab_actions::StartDeclined::err(
                self.kind(),
                agent_id,
                "bot token が空です",
            ));
        }
        self.ensure_bot_user_id(agent_id, &config.bot_token).await?;
        self.start_plan(self.plan(agent_id)?).await
    }

    async fn stop(&self, agent_id: &str) {
        self.supervisors.stop(agent_id).await;
    }

    fn is_running(&self, agent_id: &str) -> bool {
        self.extgate
            .agent_has_live_gateway(agent_id, opencrab_actions::gateway_kinds::DISCORD)
    }

    async fn restore_all(&self) {
        if let Err(error) = self.start_all().await {
            tracing::error!(error = %error, "Discord V3 restore failed");
        }
    }

    async fn shutdown_all(&self) {
        self.supervisors.shutdown_all().await;
    }
}
