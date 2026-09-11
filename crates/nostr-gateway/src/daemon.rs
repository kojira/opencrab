//! Standalone lifecycle owner for all configured instances.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use opencrab_gateway::process_supervisor::{
    GatewayChildSpawner, GatewaySupervisorSet, SupervisorConfig,
};
use opencrab_nostr::{config_from_parts, NostaroCli};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::config::{InstancePlacement, Placement};
use crate::secret::{take_master_key, MASTER_KEY_ENV};
use crate::store::{GatewayStore, InstanceRow};

const DEFAULT_RECONCILE_SECS: u64 = 5;

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Gateway-owned configuration database.
    pub database_path: PathBuf,
    /// Core-owned conversation database. Only generic instance/binding provisioning uses it.
    pub core_database_path: PathBuf,
    /// Optional one-shot source for importing pre-separation configuration.
    #[serde(default)]
    pub legacy_database_path: Option<PathBuf>,
    /// Gateway-owned local administration socket.
    pub admin_socket: PathBuf,
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
        if !self.core_database_path.is_absolute() {
            anyhow::bail!("core_database_path must be absolute");
        }
        if self.database_path == self.core_database_path {
            anyhow::bail!("database_path must differ from core_database_path");
        }
        if self
            .legacy_database_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            anyhow::bail!("legacy_database_path must be absolute");
        }
        if !self.admin_socket.is_absolute() {
            anyhow::bail!("admin_socket must be absolute");
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
    store: Arc<std::sync::Mutex<GatewayStore>>,
    core_db: opencrab_db::Db,
    cli: NostaroCli,
    secret_provider: opencrab_nostr::MainKeyProvider,
    supervisors: Arc<GatewaySupervisorSet>,
    active: BTreeMap<String, String>,
    executable: PathBuf,
    _admin_task: tokio::task::JoinHandle<Result<()>>,
}

impl Daemon {
    fn new(config: DaemonConfig) -> Result<Self> {
        let encoded_key = take_master_key()
            .with_context(|| format!("{MASTER_KEY_ENV} is required by the gateway daemon"))?;
        let master_key = Arc::new(opencrab_core::secret_box::parse_master_key(&encoded_key)?);
        let mut gateway_store = GatewayStore::open(&config.database_path)?;
        if let Some(legacy_path) = &config.legacy_database_path {
            if gateway_store.import_legacy_once(legacy_path)? {
                tracing::info!(source = %legacy_path.display(), "legacy gateway configuration imported");
            }
        }
        let encrypted = gateway_store.encrypt_plaintext_secrets(&master_key)?;
        if encrypted > 0 {
            tracing::info!(encrypted, "gateway secrets encrypted at rest");
        }
        let store = Arc::new(std::sync::Mutex::new(gateway_store));
        let secret_provider = gateway_secret_provider(store.clone(), master_key.clone());
        let core_db = opencrab_db::Db::open(
            config
                .core_database_path
                .to_str()
                .context("core_database_path must be UTF-8")?,
        )?;
        let cli = NostaroCli::new()
            .with_binary_path(config.nostaro_bin.to_string_lossy().into_owned())
            .with_workspace_base(config.workspace_base.clone())
            .with_master_key(master_key.clone())
            .with_main_key_provider(secret_provider.clone());
        std::fs::create_dir_all(&config.placement_dir)?;
        let admin_task = crate::admin::spawn(
            config.admin_socket.clone(),
            store.clone(),
            master_key.clone(),
        );
        Ok(Self {
            config,
            store,
            core_db,
            cli,
            secret_provider,
            supervisors: GatewaySupervisorSet::new(SupervisorConfig::default()),
            active: BTreeMap::new(),
            executable: std::env::current_exe().context("resolve current gateway executable")?,
            _admin_task: admin_task,
        })
    }

    async fn reconcile(&mut self) -> Result<()> {
        let rows = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?
            .list_enabled()?;
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

    async fn reconcile_agent(&mut self, row: &InstanceRow) -> Result<()> {
        if row.secret_key.trim().is_empty() {
            anyhow::bail!("configured instance has no secret key");
        }
        let config = config_from_parts(&row.relays_json, &row.filter_json);
        NostaroCli::materialize_config(&row.agent_id, &config.effective_relays(), None)?;
        let self_pubkey = self.cli.pubkey(&row.agent_id).await?.trim().to_string();
        let followees = self.cli.fetch_following(&row.agent_id).await?;
        let (watches, access) = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?;
            let watches = store.watches(&row.agent_id)?;
            let keys = store.allow_keys(&row.agent_id)?;
            (
                watches,
                opencrab_nostr::gate_provision::build_allow_sources(followees, &keys),
            )
        };
        let plan = {
            self.store
                .lock()
                .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?
                .set_self_pubkey(&row.agent_id, &self_pubkey)?;
            let mut conn = self
                .core_db
                .lock()
                .map_err(|_| anyhow::anyhow!("core database lock poisoned"))?;
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

fn gateway_secret_provider(
    store: Arc<std::sync::Mutex<GatewayStore>>,
    master_key: opencrab_nostr::MasterKey,
) -> opencrab_nostr::MainKeyProvider {
    Arc::new(move |agent_id: &str| {
        let secret = store
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?
            .get(agent_id)?
            .map(|row| row.secret_key)
            .with_context(|| format!("gateway instance {agent_id} is not configured"))?;
        if secret.trim().is_empty() {
            anyhow::bail!("gateway instance has no secret key");
        }
        if opencrab_core::secret_box::is_encrypted(&secret) {
            let bytes = opencrab_core::secret_box::decrypt(&secret, &master_key)?;
            Ok(zeroize::Zeroizing::new(
                String::from_utf8(bytes.to_vec()).context("decrypted gateway key is not UTF-8")?,
            ))
        } else {
            Ok(zeroize::Zeroizing::new(secret))
        }
    })
}

fn now_nanos() -> Result<i64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before Unix epoch")?;
    i64::try_from(elapsed.as_nanos()).context("current time does not fit i64 nanoseconds")
}

fn fingerprint(row: &InstanceRow, config_b64: &str) -> String {
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
            core_database_path: "/tmp/core.db".into(),
            legacy_database_path: None,
            admin_socket: "/tmp/nostr-admin.sock".into(),
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
