//! Standalone lifecycle owner for all configured instances.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use opencrab_gateway::process_supervisor::{
    GatewayChildSpawner, GatewaySupervisorSet, SupervisorConfig,
};
use opencrab_nostr::{config_from_row, NostaroCli};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::config::{InstancePlacement, Placement};
use crate::secret::{take_master_key, MASTER_KEY_ENV};

const DEFAULT_RECONCILE_SECS: u64 = 5;

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    pub database_path: PathBuf,
    pub core_socket: PathBuf,
    pub nostaro_bin: PathBuf,
    #[serde(default = "default_placement_dir")]
    pub placement_dir: PathBuf,
    #[serde(default = "default_workspace_base")]
    pub workspace_base: String,
    #[serde(default = "default_reconcile_secs")]
    pub reconcile_secs: u64,
}

fn default_placement_dir() -> PathBuf {
    PathBuf::from("data/gate/nostr")
}

fn default_workspace_base() -> String {
    "data/agents/{agent_id}/workspace".to_string()
}

fn default_reconcile_secs() -> u64 {
    DEFAULT_RECONCILE_SECS
}

impl DaemonConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("daemon config read failed: {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes).context("daemon config must be JSON")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.database_path.is_absolute() {
            anyhow::bail!("database_path must be absolute");
        }
        if !self.core_socket.is_absolute() {
            anyhow::bail!("core_socket must be absolute");
        }
        if self.nostaro_bin.as_os_str().is_empty() {
            anyhow::bail!("nostaro_bin must be nonempty");
        }
        if self.reconcile_secs == 0 {
            anyhow::bail!("reconcile_secs must be positive");
        }
        Ok(())
    }
}

struct Daemon {
    config: DaemonConfig,
    db: opencrab_db::Db,
    cli: NostaroCli,
    secret_provider: opencrab_nostr::MainKeyProvider,
    supervisors: Arc<GatewaySupervisorSet>,
    active: BTreeMap<String, String>,
    executable: PathBuf,
}

impl Daemon {
    fn new(config: DaemonConfig) -> Result<Self> {
        let encoded_key = take_master_key()
            .with_context(|| format!("{MASTER_KEY_ENV} is required by the gateway daemon"))?;
        let master_key = Arc::new(opencrab_core::secret_box::parse_master_key(&encoded_key)?);
        let db = opencrab_db::Db::open(
            config
                .database_path
                .to_str()
                .context("database_path must be UTF-8")?,
        )?;
        let report = opencrab_nostr::secret_migration::migrate_nostr_secrets_at_rest(
            &db,
            &master_key,
            Path::new("data/agents"),
        );
        if report.changed_anything() {
            tracing::info!(?report, "gateway-owned secret migration completed");
        }
        let secret_provider = opencrab_nostr::db_main_key_provider(db.clone(), master_key.clone());
        let cli = NostaroCli::new()
            .with_binary_path(config.nostaro_bin.to_string_lossy().into_owned())
            .with_workspace_base(config.workspace_base.clone())
            .with_master_key(master_key)
            .with_main_key_provider(secret_provider.clone());
        std::fs::create_dir_all(&config.placement_dir)?;
        Ok(Self {
            config,
            db,
            cli,
            secret_provider,
            supervisors: GatewaySupervisorSet::new(SupervisorConfig::default()),
            active: BTreeMap::new(),
            executable: std::env::current_exe().context("resolve current gateway executable")?,
        })
    }

    async fn reconcile(&mut self) -> Result<()> {
        let rows = {
            let conn = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            opencrab_db::queries::list_enabled_agent_nostr_configs(&conn)?
        };
        let desired: BTreeSet<String> = rows.iter().map(|row| row.agent_id.clone()).collect();
        let stopped: Vec<String> = self
            .active
            .keys()
            .filter(|agent_id| !desired.contains(*agent_id))
            .cloned()
            .collect();
        for agent_id in stopped {
            self.supervisors.stop(&agent_id).await;
            self.active.remove(&agent_id);
        }
        for row in rows {
            if let Err(error) = self.reconcile_agent(&row).await {
                self.supervisors.stop(&row.agent_id).await;
                self.active.remove(&row.agent_id);
                tracing::error!(agent_id = %row.agent_id, error = %format!("{error:#}"), "instance reconciliation failed");
            }
        }
        Ok(())
    }

    async fn reconcile_agent(
        &mut self,
        row: &opencrab_db::queries::AgentNostrConfigRow,
    ) -> Result<()> {
        if row.secret_key.trim().is_empty() {
            anyhow::bail!("configured instance has no secret key");
        }
        let config = config_from_row(row);
        NostaroCli::materialize_config(&row.agent_id, &config.effective_relays(), None)?;
        let self_pubkey = self.cli.pubkey(&row.agent_id).await?.trim().to_string();
        let followees = self.cli.fetch_following(&row.agent_id).await?;
        let (watches, access) = {
            let conn = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            let watches =
                opencrab_db::queries::list_session_watches_for_agent(&conn, &row.agent_id)?;
            let keys = opencrab_nostr::gate_provision::load_gate_allow_keys(&conn, &row.agent_id)?;
            (
                watches,
                opencrab_nostr::gate_provision::build_allow_sources(followees, &keys),
            )
        };
        let plan = {
            let mut conn = self
                .db
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            opencrab_db::queries::set_agent_nostr_self_pubkey(&conn, &row.agent_id, &self_pubkey)?;
            opencrab_nostr::gate_provision::provision_nostr_gate(
                &mut conn,
                &row.agent_id,
                &self_pubkey,
                &config,
                &watches,
                &access,
                now_nanos()?,
            )?;
            opencrab_nostr::gate_provision::load_nostr_placement_plan(&conn, &row.agent_id)?
        };
        let fingerprint = fingerprint(row, &plan.config_b64);
        if self.active.get(&row.agent_id) == Some(&fingerprint) {
            return Ok(());
        }
        let placement = Placement {
            core_socket: self.config.core_socket.to_string_lossy().into_owned(),
            nostaro_bin: self.config.nostaro_bin.to_string_lossy().into_owned(),
            instances: vec![InstancePlacement {
                instance_id: plan.instance_id,
                revision: plan.revision,
                address: plan.address,
                config_b64: plan.config_b64,
            }],
        };
        let path = self
            .config
            .placement_dir
            .join(format!("{}.json", row.agent_id));
        write_placement(&path, &placement)?;
        let secret = (self.secret_provider)(&row.agent_id)?;
        let spawner = Arc::new(GatewayChildSpawner::with_secret_env(
            self.executable.clone(),
            path,
            secret.to_string(),
            crate::secret::SECRET_ENV,
            "nostr-gateway-instance",
            row.agent_id.clone(),
        ));
        self.supervisors.start(&row.agent_id, spawner).await;
        self.active.insert(row.agent_id.clone(), fingerprint);
        Ok(())
    }
}

fn now_nanos() -> Result<i64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;
    i64::try_from(elapsed.as_nanos()).context("current time does not fit i64 nanoseconds")
}

fn fingerprint(row: &opencrab_db::queries::AgentNostrConfigRow, config_b64: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(row.secret_key.as_bytes());
    hash.update([0]);
    hash.update(config_b64.as_bytes());
    format!("{:x}", hash.finalize())
}

fn write_placement(path: &Path, placement: &Placement) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(placement)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

pub async fn run(config: DaemonConfig) -> Result<()> {
    let mut daemon = Daemon::new(config)?;
    let period = Duration::from_secs(daemon.config.reconcile_secs);
    let mut ticker = tokio::time::interval(period);
    loop {
        tokio::select! {
            _ = ticker.tick() => daemon.reconcile().await?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("shutdown signal")?;
                daemon.supervisors.shutdown_all().await;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_config_requires_absolute_database_and_socket() {
        let config = DaemonConfig {
            database_path: "relative.db".into(),
            core_socket: "/tmp/gate.sock".into(),
            nostaro_bin: "nostaro".into(),
            placement_dir: "data/gate/nostr".into(),
            workspace_base: default_workspace_base(),
            reconcile_secs: 5,
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("database_path"));
        let config = DaemonConfig {
            database_path: "/tmp/opencrab.db".into(),
            core_socket: "relative.sock".into(),
            ..config
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("core_socket"));
    }
}
