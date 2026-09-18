//! Standalone lifecycle owner for all configured instances.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
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
        let (existing, desired_config_b64) = {
            let conn = self
                .core_db
                .lock()
                .map_err(|_| anyhow::anyhow!("core database lock poisoned"))?;
            let desired = opencrab_nostr::gate_provision::desired_nostr_config_b64(
                &conn,
                &row.agent_id,
                &self_pubkey,
                &config,
                &watches,
                &access,
            )?;
            let existing =
                opencrab_nostr::gate_provision::find_nostr_placement_plan(&conn, &row.agent_id)?;
            (existing, desired)
        };
        let fingerprint = fingerprint(row, &desired_config_b64);
        let steps = reconciliation_steps(
            existing.as_ref(),
            self.active.get(&row.agent_id).map(String::as_str),
            &fingerprint,
            &desired_config_b64,
        );
        if steps.is_empty() {
            return Ok(());
        }

        let mut self_pubkey_stored = false;
        for step in steps {
            match step {
                ReconcileStep::Stop => {
                    self.supervisors.stop(&row.agent_id).await;
                    self.active.remove(&row.agent_id);
                }
                ReconcileStep::Provision | ReconcileStep::Revise => {
                    self.store
                        .lock()
                        .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?
                        .set_self_pubkey(&row.agent_id, &self_pubkey)?;
                    self_pubkey_stored = true;
                    let mut conn = self
                        .core_db
                        .lock()
                        .map_err(|_| anyhow::anyhow!("core database lock poisoned"))?;
                    match step {
                        ReconcileStep::Provision => {
                            opencrab_nostr::gate_provision::provision_nostr_gate(
                                &mut conn,
                                &row.agent_id,
                                &self_pubkey,
                                &config,
                                &watches,
                                &access,
                                now_nanos()?,
                            )?;
                        }
                        ReconcileStep::Revise => {
                            opencrab_nostr::gate_provision::revise_nostr_gate(
                                &mut conn,
                                &row.agent_id,
                                &self_pubkey,
                                &config,
                                &watches,
                                &access,
                                opencrab_nostr::gate_provision::StoppedRevision {
                                    expected_revision: existing
                                        .as_ref()
                                        .context("revision requested without existing placement")?
                                        .revision,
                                    updated_at: now_nanos()?,
                                },
                            )?;
                        }
                        _ => unreachable!(),
                    }
                }
                ReconcileStep::Start => {
                    if !self_pubkey_stored {
                        self.store
                            .lock()
                            .map_err(|_| anyhow::anyhow!("gateway store lock poisoned"))?
                            .set_self_pubkey(&row.agent_id, &self_pubkey)?;
                    }
                    let plan = {
                        let conn = self
                            .core_db
                            .lock()
                            .map_err(|_| anyhow::anyhow!("core database lock poisoned"))?;
                        opencrab_nostr::gate_provision::load_nostr_placement_plan(
                            &conn,
                            &row.agent_id,
                        )?
                    };
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
                    self.active
                        .insert(row.agent_id.clone(), fingerprint.clone());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileStep {
    Stop,
    Provision,
    Revise,
    Start,
}

fn reconciliation_steps(
    existing: Option<&opencrab_nostr::gate_provision::NostrPlacementPlan>,
    active_fingerprint: Option<&str>,
    desired_fingerprint: &str,
    desired_config_b64: &str,
) -> Vec<ReconcileStep> {
    let config_changed = existing.is_some_and(|plan| plan.config_b64 != desired_config_b64);
    if !config_changed && active_fingerprint == Some(desired_fingerprint) {
        return Vec::new();
    }

    let mut steps = Vec::with_capacity(3);
    if active_fingerprint.is_some() {
        steps.push(ReconcileStep::Stop);
    }
    steps.push(match existing {
        None => ReconcileStep::Provision,
        Some(_) if config_changed => ReconcileStep::Revise,
        Some(_) => ReconcileStep::Start,
    });
    if !matches!(steps.last(), Some(ReconcileStep::Start)) {
        steps.push(ReconcileStep::Start);
    }
    steps
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

fn shutdown_signal() -> Result<impl Future<Output = Result<()>>> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    Ok(async move {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => signal.context("SIGINT shutdown signal"),
            signal = terminate.recv() => {
                signal.context("SIGTERM signal stream closed")?;
                Ok(())
            }
        }
    })
}

async fn shutdown_supervisors_on_exit<F>(
    supervisors: Arc<GatewaySupervisorSet>,
    work: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    let outcome = work.await;
    // Always await every identity owner, including replacements created by reconciliation.
    supervisors.shutdown_all().await;
    outcome
}

pub async fn run(config: DaemonConfig) -> Result<()> {
    let mut daemon = Daemon::new(config)?;
    let shutdown = shutdown_signal()?;
    tokio::pin!(shutdown);
    let supervisors = daemon.supervisors.clone();
    shutdown_supervisors_on_exit(supervisors, async move {
        let period = Duration::from_secs(daemon.config.reconcile_secs);
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => daemon.reconcile().await?,
                signal = &mut shutdown => {
                    signal?;
                    return Ok(());
                }
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_gateway::process_supervisor::ChildSpawner as _;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    const SIGNAL_FIXTURE_ENV: &str = "OPENCRAB_NOSTR_SHUTDOWN_SIGNAL_FIXTURE";
    const SCRIPT_ENV: &str = "OPENCRAB_NOSTR_SHUTDOWN_SCRIPT";
    const PID_FILE_ENV: &str = "OPENCRAB_NOSTR_SHUTDOWN_PID_FILE";
    const READY_FILE_ENV: &str = "OPENCRAB_NOSTR_SHUTDOWN_READY_FILE";
    const PID_OUTPUT_ENV: &str = "OPENCRAB_TEST_PID_FILE";

    struct ChildGuard(Option<Child>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    struct ProcessGuard(Vec<i32>);

    impl Drop for ProcessGuard {
        fn drop(&mut self) {
            for pid in &self.0 {
                // SAFETY: the test records positive pids for processes that it spawned.
                let _ = unsafe { libc::kill(*pid, libc::SIGKILL) };
            }
        }
    }

    async fn wait_for_path(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn read_pids(path: &Path) -> (i32, i32) {
        let text = std::fs::read_to_string(path).unwrap();
        let mut fields = text.split_whitespace().map(|field| field.parse().unwrap());
        (fields.next().unwrap(), fields.next().unwrap())
    }

    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 only probes the positive pid and has no side effects.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn assert_processes_gone(pids: (i32, i32)) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while (process_exists(pids.0) || process_exists(pids.1)) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !process_exists(pids.0),
            "gateway child {} survived shutdown",
            pids.0
        );
        assert!(
            !process_exists(pids.1),
            "gateway descendant {} survived shutdown",
            pids.1
        );
    }

    async fn replace_process_tree(
        supervisors: &Arc<GatewaySupervisorSet>,
        script: PathBuf,
        pid_file: &Path,
    ) {
        let spawner = Arc::new(GatewayChildSpawner::with_secret_env(
            PathBuf::from("/bin/sh"),
            script,
            pid_file.to_string_lossy().into_owned(),
            PID_OUTPUT_ENV,
            "nostr-gateway-test",
            "identity-a".into(),
        ));
        // Exercise the same public spawner used by reconciliation, not a fake child.
        assert_eq!(spawner.agent_id(), "identity-a");
        supervisors.start("identity-a", spawner).await;
        wait_for_path(pid_file).await;
    }

    async fn start_process_tree(script: PathBuf, pid_file: &Path) -> Arc<GatewaySupervisorSet> {
        let supervisors = GatewaySupervisorSet::new(SupervisorConfig::default());
        replace_process_tree(&supervisors, script, pid_file).await;
        supervisors
    }

    fn assert_isolated_process_group(pids: (i32, i32)) {
        // SAFETY: getpgid only inspects the live positive pids written by the fixture.
        let gateway_group = unsafe { libc::getpgid(pids.0) };
        let descendant_group = unsafe { libc::getpgid(pids.1) };
        assert_eq!(
            gateway_group, pids.0,
            "gateway must lead its isolated process group"
        );
        assert_eq!(
            descendant_group, gateway_group,
            "descendant must inherit the group"
        );
        // SAFETY: getpgrp has no preconditions.
        assert_ne!(gateway_group, unsafe { libc::getpgrp() });
    }

    async fn signal_fixture() {
        let script = PathBuf::from(std::env::var_os(SCRIPT_ENV).unwrap());
        let pid_file = PathBuf::from(std::env::var_os(PID_FILE_ENV).unwrap());
        let ready_file = PathBuf::from(std::env::var_os(READY_FILE_ENV).unwrap());
        // Register SIGTERM before publishing readiness; SIGINT is registered on first poll below.
        let signal = shutdown_signal().unwrap();
        let supervisors = start_process_tree(script, &pid_file).await;
        std::fs::write(ready_file, b"ready").unwrap();
        shutdown_supervisors_on_exit(supervisors, signal)
            .await
            .unwrap();
    }

    async fn run_signal_case(signal: i32) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gateway-tree.sh");
        let pid_file = dir.path().join("pids");
        let ready_file = dir.path().join("ready");
        std::fs::write(
            &script,
            "(trap '' TERM INT; while :; do sleep 60; done) &\n\
             descendant=$!\n\
             printf '%s %s\\n' \"$$\" \"$descendant\" > \"$OPENCRAB_TEST_PID_FILE\"\n\
             wait \"$descendant\"\n",
        )
        .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::tests::daemon_signals_reap_isolated_gateway_process_group",
                "--nocapture",
            ])
            .env(SIGNAL_FIXTURE_ENV, "1")
            .env(SCRIPT_ENV, &script)
            .env(PID_FILE_ENV, &pid_file)
            .env(READY_FILE_ENV, &ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut owner = ChildGuard(Some(child));
        wait_for_path(&ready_file).await;
        let pids = read_pids(&pid_file);
        let process_guard = ProcessGuard(vec![pids.0, pids.1]);
        assert_isolated_process_group(pids);

        let owner_pid = owner.0.as_ref().unwrap().id() as i32;
        // SAFETY: owner_pid is the live subprocess created above.
        assert_eq!(unsafe { libc::kill(owner_pid, signal) }, 0);
        let deadline = Instant::now() + Duration::from_secs(8);
        let status = loop {
            if let Some(status) = owner.0.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "signal fixture did not exit");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        owner.0 = None;
        assert!(status.success(), "signal fixture exited with {status}");
        assert_processes_gone(pids).await;
        drop(process_guard);
    }

    fn placement(config_b64: &str) -> opencrab_nostr::gate_provision::NostrPlacementPlan {
        opencrab_nostr::gate_provision::NostrPlacementPlan {
            agent_id: "agent".into(),
            instance_id: "instance".into(),
            revision: 7,
            address: "address".into(),
            config_b64: config_b64.into(),
        }
    }

    #[test]
    fn changed_active_instance_stops_before_revision_and_restart() {
        assert_eq!(
            reconciliation_steps(
                Some(&placement("old")),
                Some("old-fingerprint"),
                "new-fingerprint",
                "new"
            ),
            [
                ReconcileStep::Stop,
                ReconcileStep::Revise,
                ReconcileStep::Start
            ]
        );
    }

    #[test]
    fn unchanged_active_instance_does_not_churn() {
        assert!(reconciliation_steps(
            Some(&placement("same")),
            Some("same-fingerprint"),
            "same-fingerprint",
            "same"
        )
        .is_empty());
    }

    #[test]
    fn absent_instance_is_provisioned_before_start() {
        assert_eq!(
            reconciliation_steps(None, None, "new-fingerprint", "new"),
            [ReconcileStep::Provision, ReconcileStep::Start]
        );
    }

    #[test]
    fn secret_only_change_stops_without_revising_core_config() {
        assert_eq!(
            reconciliation_steps(
                Some(&placement("same")),
                Some("old-fingerprint"),
                "new-fingerprint",
                "same"
            ),
            [ReconcileStep::Stop, ReconcileStep::Start]
        );
    }

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

    #[tokio::test]
    async fn normal_daemon_exit_reaps_isolated_gateway_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gateway-tree.sh");
        let first_pid_file = dir.path().join("first-pids");
        let replacement_pid_file = dir.path().join("replacement-pids");
        std::fs::write(
            &script,
            "(trap '' TERM INT; while :; do sleep 60; done) &\n\
             descendant=$!\n\
             printf '%s %s\\n' \"$$\" \"$descendant\" > \"$OPENCRAB_TEST_PID_FILE\"\n\
             wait \"$descendant\"\n",
        )
        .unwrap();
        let supervisors = start_process_tree(script.clone(), &first_pid_file).await;
        let first_pids = read_pids(&first_pid_file);
        let mut process_guard = ProcessGuard(vec![first_pids.0, first_pids.1]);
        assert_isolated_process_group(first_pids);

        replace_process_tree(&supervisors, script, &replacement_pid_file).await;
        assert_processes_gone(first_pids).await;
        let replacement_pids = read_pids(&replacement_pid_file);
        process_guard
            .0
            .extend([replacement_pids.0, replacement_pids.1]);
        assert_isolated_process_group(replacement_pids);

        shutdown_supervisors_on_exit(supervisors, async { Ok(()) })
            .await
            .unwrap();
        assert_processes_gone(replacement_pids).await;
        drop(process_guard);
    }

    #[tokio::test]
    async fn daemon_signals_reap_isolated_gateway_process_group() {
        if std::env::var_os(SIGNAL_FIXTURE_ENV).is_some() {
            signal_fixture().await;
            return;
        }
        run_signal_case(libc::SIGTERM).await;
        run_signal_case(libc::SIGINT).await;
    }
}
