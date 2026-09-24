//! Nostr-owned lifecycle daemon. Core is reached only through a generic control trait.

use crate::store::{InstanceRow, NostrStore};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use opencrab_process_supervisor::{
    ChildSpawner, ProcessObserver, ProcessSupervisorSet, SupervisorConfig,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub database_path: PathBuf,
    pub admin_socket: PathBuf,
    #[serde(default)]
    pub admin_instance_ids: Vec<String>,
    pub gate_admin_socket: PathBuf,
    pub gate_admin_credential: PathBuf,
    pub core_socket: PathBuf,
    pub child_binary: PathBuf,
    pub placement_dir: PathBuf,
    pub nostaro_bin: PathBuf,
    #[serde(default = "default_reconcile_millis")]
    pub reconcile_millis: u64,
}

fn default_reconcile_millis() -> u64 {
    1_000
}

fn retry_deadline(row: &InstanceRow) -> i64 {
    let shift = row.failure_count.min(6);
    let delay = 1_000_i64.saturating_mul(1_i64 << shift).min(60_000);
    chrono::Utc::now().timestamp_millis().saturating_add(delay)
}

impl DaemonConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("database_path", &self.database_path),
            ("admin_socket", &self.admin_socket),
            ("gate_admin_socket", &self.gate_admin_socket),
            ("gate_admin_credential", &self.gate_admin_credential),
            ("core_socket", &self.core_socket),
            ("child_binary", &self.child_binary),
            ("placement_dir", &self.placement_dir),
            ("nostaro_bin", &self.nostaro_bin),
        ] {
            anyhow::ensure!(path.is_absolute(), "{name} must be absolute");
        }
        anyhow::ensure!(
            self.reconcile_millis > 0,
            "reconcile_millis must be positive"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedInstance {
    pub revision: u64,
    pub digest: String,
    pub bindings: Vec<String>,
    pub enabled: bool,
}

#[async_trait]
pub trait GateReconciler: Send + Sync {
    async fn observe(
        &self,
        desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation>;

    async fn reconcile(
        &self,
        desired: &InstanceRow,
        subject_grant: Option<&str>,
    ) -> Result<VerifiedInstance>;
}

pub struct UdsGateReconciler {
    client: opencrab_gate_client::admin::GateAdminClient,
    kind_id: String,
}

impl UdsGateReconciler {
    pub fn new(
        client: opencrab_gate_client::admin::GateAdminClient,
        kind_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            kind_id: kind_id.into(),
        }
    }
}

#[async_trait]
impl GateReconciler for UdsGateReconciler {
    async fn observe(
        &self,
        desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        use opencrab_process_supervisor::lifecycle::CoreObservation;
        let Some(observed) = self.client.get_instance(&desired.instance_id).await? else {
            return Ok(CoreObservation::Mismatch);
        };
        let mut bindings: Vec<_> = observed
            .bindings
            .into_iter()
            .map(|value| value.binding_id)
            .collect();
        bindings.sort();
        let mut expected = desired.core_bindings.clone();
        expected.sort();
        if observed.revision == desired.core_revision.unwrap_or(0)
            && observed.config_digest == desired.core_digest.as_deref().unwrap_or_default()
            && bindings == expected
        {
            Ok(if observed.enabled {
                CoreObservation::ExactEnabled
            } else {
                CoreObservation::ExactDisabled
            })
        } else {
            Ok(CoreObservation::Mismatch)
        }
    }

    async fn reconcile(
        &self,
        desired: &InstanceRow,
        subject_grant: Option<&str>,
    ) -> Result<VerifiedInstance> {
        let observed = self
            .client
            .reconcile(opencrab_gate_client::admin::ReconcileDesired {
                instance_id: &desired.instance_id,
                kind_id: &self.kind_id,
                subject_id: desired.subject_id,
                enabled: desired.enabled,
                config_b64: &desired.config_b64,
                subject_grant,
                addresses: &desired.addresses,
            })
            .await?;
        Ok(VerifiedInstance {
            revision: observed.revision,
            digest: observed.config_digest,
            bindings: observed
                .bindings
                .into_iter()
                .map(|binding| binding.binding_id)
                .collect(),
            enabled: observed.enabled,
        })
    }
}

pub trait SpawnerFactory: Send + Sync {
    fn observe_process(
        &self,
        _row: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
        Ok(opencrab_process_supervisor::lifecycle::ProcessObservation::Missing)
    }
    fn reap_stale(&self, _row: &InstanceRow) -> Result<()> {
        Ok(())
    }
    fn cleanup_artifacts(&self, _row: &InstanceRow) -> Result<()> {
        Ok(())
    }
    fn adopted_child(
        &self,
        _row: &InstanceRow,
    ) -> Result<Option<Box<dyn opencrab_process_supervisor::SupervisedChild>>> {
        Ok(None)
    }
    fn control_socket(&self, _row: &InstanceRow, _start_nonce: &str) -> Option<PathBuf> {
        None
    }
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        start_nonce: &str,
    ) -> Result<Arc<dyn ChildSpawner>>;
}

struct StoreObserver {
    store: Arc<Mutex<NostrStore>>,
    generation: u64,
    nonce: String,
}

#[async_trait]
impl ProcessObserver for StoreObserver {
    async fn spawned(&self, target_id: &str, pid: Option<u32>) {
        if let Some(pid) = pid {
            let _ = self.store.lock().ok().and_then(|store| {
                store
                    .record_started(target_id, self.generation, pid, &self.nonce)
                    .ok()
            });
        }
    }

    async fn spawn_failed(&self, target_id: &str) {
        self.persist_error(target_id, "child_start_exit");
    }

    async fn exited(&self, target_id: &str, _: &str) {
        self.persist_error(target_id, "child_exit");
    }
}

impl StoreObserver {
    fn persist_error(&self, target_id: &str, code: &str) {
        let retry = chrono::Utc::now().timestamp_millis() + 1_000;
        if let Ok(store) = self.store.lock() {
            let _ = store.mark_error(target_id, self.generation, code, retry);
        }
    }
}

pub struct NostrDaemon<R, F> {
    store: Arc<Mutex<NostrStore>>,
    key: Arc<[u8; 32]>,
    reconciler: Arc<R>,
    factory: Arc<F>,
    supervisors: Arc<ProcessSupervisorSet>,
    startup_recovered: std::sync::atomic::AtomicBool,
}

impl<R, F> NostrDaemon<R, F>
where
    R: GateReconciler + 'static,
    F: SpawnerFactory + 'static,
{
    pub fn new(
        store: NostrStore,
        key: [u8; 32],
        reconciler: Arc<R>,
        factory: Arc<F>,
    ) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            key: Arc::new(key),
            reconciler,
            factory,
            supervisors: ProcessSupervisorSet::new(SupervisorConfig::daemon_owned()),
            startup_recovered: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn store(&self) -> Arc<Mutex<NostrStore>> {
        self.store.clone()
    }

    async fn recover_startup(&self) -> Result<()> {
        use opencrab_process_supervisor::lifecycle::{
            CoreObservation, ProcessObservation, StartupDecision,
        };
        let rows = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .list()?;
        let now = chrono::Utc::now().timestamp_millis();
        for row in rows {
            let process = self
                .factory
                .observe_process(&row)
                .unwrap_or(ProcessObservation::Unknown);
            let core = self
                .reconciler
                .observe(&row)
                .await
                .unwrap_or(CoreObservation::Unavailable);
            let decision = row.lifecycle().recover(now, process, core);
            if !matches!(decision, StartupDecision::AdoptRunning) {
                let _ = self.factory.reap_stale(&row);
                let _ = self.factory.cleanup_artifacts(&row);
            }
            match decision {
                StartupDecision::KeepDisabled => {}
                StartupDecision::EnqueuePending => {
                    self.store
                        .lock()
                        .unwrap()
                        .mark_pending(&row.instance_id, row.desired_generation)?;
                }
                StartupDecision::StartReady => {}
                StartupDecision::AdoptRunning => {
                    let child = self
                        .factory
                        .adopted_child(&row)?
                        .context("exact live process could not be adopted")?;
                    self.supervisors
                        .adopt_observed(
                            &row.instance_id,
                            child,
                            Arc::new(StoreObserver {
                                store: self.store.clone(),
                                generation: row.desired_generation,
                                nonce: row.process_nonce.clone().unwrap_or_default(),
                            }),
                        )
                        .await;
                }
                StartupDecision::PersistError(code) => {
                    self.store.lock().unwrap().mark_error(
                        &row.instance_id,
                        row.desired_generation,
                        code,
                        retry_deadline(&row),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub async fn reconcile_once(&self) -> Result<()> {
        if !self
            .startup_recovered
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.recover_startup().await?;
        }
        let rows = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .list()?;
        for row in rows {
            match row.lifecycle_state {
                opencrab_process_supervisor::lifecycle::LifecycleState::Pending => {
                    self.supervisors.stop(&row.instance_id).await;
                    self.factory.reap_stale(&row)?;
                    self.factory.cleanup_artifacts(&row)?;
                    if !self
                        .store
                        .lock()
                        .unwrap()
                        .mark_provisioning(&row.instance_id, row.desired_generation)?
                    {
                        continue;
                    }
                    let grant = self
                        .store
                        .lock()
                        .unwrap()
                        .decrypt_subject_grant(&row.instance_id, &self.key)?;
                    let grant_text = grant
                        .as_ref()
                        .map(|value| {
                            std::str::from_utf8(value).context("subject grant is not UTF-8")
                        })
                        .transpose()?;
                    let verified = match self.reconciler.reconcile(&row, grant_text).await {
                        Ok(value) => value,
                        Err(error) => {
                            self.factory.cleanup_artifacts(&row)?;
                            self.store.lock().unwrap().mark_error(
                                &row.instance_id,
                                row.desired_generation,
                                "reconciliation_failed",
                                retry_deadline(&row),
                            )?;
                            tracing::warn!(instance_id = %row.instance_id, error = %error, "instance reconciliation failed");
                            continue;
                        }
                    };
                    anyhow::ensure!(
                        self.store.lock().unwrap().mark_verified(
                            &row.instance_id,
                            row.desired_generation,
                            verified.revision,
                            &verified.digest,
                            &verified.bindings,
                            verified.enabled,
                        )?,
                        "stale lifecycle verification commit"
                    );
                    self.store
                        .lock()
                        .unwrap()
                        .clear_subject_grant(&row.instance_id, row.desired_generation)?;
                    if verified.enabled {
                        if let Err(error) = self.start_ready(&row.instance_id).await {
                            self.factory.cleanup_artifacts(&row)?;
                            self.store.lock().unwrap().mark_error(
                                &row.instance_id,
                                row.desired_generation,
                                "child_start_failed",
                                retry_deadline(&row),
                            )?;
                            tracing::warn!(instance_id = %row.instance_id, error = %error, "child start failed");
                        }
                    } else {
                        self.factory.cleanup_artifacts(&row)?;
                    }
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Ready => {
                    if let Err(error) = self.start_ready(&row.instance_id).await {
                        self.factory.cleanup_artifacts(&row)?;
                        self.store.lock().unwrap().mark_error(
                            &row.instance_id,
                            row.desired_generation,
                            "child_start_failed",
                            retry_deadline(&row),
                        )?;
                        tracing::warn!(instance_id = %row.instance_id, error = %error, "child start failed");
                    }
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Disabled
                | opencrab_process_supervisor::lifecycle::LifecycleState::Provisioning => {
                    self.supervisors.stop(&row.instance_id).await;
                    self.factory.reap_stale(&row)?;
                    self.factory.cleanup_artifacts(&row)?;
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Error => {
                    self.supervisors.stop(&row.instance_id).await;
                    self.factory.reap_stale(&row)?;
                    self.factory.cleanup_artifacts(&row)?;
                    let _ = self.store.lock().unwrap().retry_due_error(
                        &row.instance_id,
                        row.desired_generation,
                        chrono::Utc::now().timestamp_millis(),
                    )?;
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Running => {}
            }
        }
        Ok(())
    }

    async fn start_ready(&self, instance_id: &str) -> Result<()> {
        let row = self
            .store
            .lock()
            .unwrap()
            .get(instance_id)?
            .context("instance missing")?;
        if !row.lifecycle().child_may_start() || row.process_nonce.is_some() {
            return Ok(());
        }
        let credential = self
            .store
            .lock()
            .unwrap()
            .decrypt_credential(instance_id, &self.key)?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let readiness = match self.factory.control_socket(&row, &nonce) {
            Some(path) => Some(prepare_readiness_listener(&path)?),
            None => None,
        };
        let spawner = self.factory.for_instance(&row, credential, &nonce)?;
        let observer = Arc::new(StoreObserver {
            store: self.store.clone(),
            generation: row.desired_generation,
            nonce: nonce.clone(),
        });
        self.supervisors
            .start_observed(instance_id, spawner, observer)
            .await;
        if let Some(listener) = readiness {
            let store = self.store.clone();
            let id = row.instance_id.clone();
            let generation = row.desired_generation;
            tokio::spawn(wait_for_child_readiness(
                listener,
                store,
                self.supervisors.clone(),
                self.factory.clone(),
                id,
                generation,
                nonce,
            ));
        }
        Ok(())
    }

    pub fn confirm_child_ready(
        &self,
        instance_id: &str,
        generation: u64,
        pid: u32,
        nonce: &str,
    ) -> Result<bool> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .mark_running(instance_id, generation, pid, nonce)
    }

    pub async fn shutdown(&self) {
        self.supervisors.shutdown_all().await;
    }
}

fn prepare_readiness_listener(path: &Path) -> Result<tokio::net::UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

async fn wait_for_child_readiness<F: SpawnerFactory + 'static>(
    listener: tokio::net::UnixListener,
    store: Arc<Mutex<NostrStore>>,
    supervisors: Arc<ProcessSupervisorSet>,
    factory: Arc<F>,
    instance_id: String,
    generation: u64,
    expected_nonce: String,
) {
    use tokio::io::AsyncReadExt as _;
    let ready = async {
        let (mut stream, _) = listener.accept().await?;
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let nonce = value
            .get("nonce")
            .and_then(serde_json::Value::as_str)
            .context("readiness nonce missing")?;
        let pid = value
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .context("readiness pid missing")?;
        anyhow::ensure!(nonce == expected_nonce, "readiness nonce mismatch");
        Ok::<(u32, String), anyhow::Error>((pid, nonce.to_string()))
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), ready).await;
    let parsed = match result {
        Ok(Ok(value)) => value,
        _ => {
            supervisors.stop(&instance_id).await;
            if let Ok(guard) = store.lock() {
                if let Ok(Some(row)) = guard.get(&instance_id) {
                    let _ = factory.cleanup_artifacts(&row);
                    let _ = guard.mark_error(
                        &instance_id,
                        generation,
                        "child_readiness_failed",
                        retry_deadline(&row),
                    );
                }
            }
            return;
        }
    };
    if let Ok(store) = store.lock() {
        let _ = store.mark_running(&instance_id, generation, parsed.0, &parsed.1);
    }
}

struct ProductionFactory {
    child_binary: PathBuf,
    core_socket: PathBuf,
    placement_dir: PathBuf,
    nostaro_bin: PathBuf,
}

impl SpawnerFactory for ProductionFactory {
    fn observe_process(
        &self,
        row: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
        use opencrab_process_supervisor::lifecycle::ProcessObservation;
        let Some(pid) = row.process_id else {
            return Ok(ProcessObservation::Missing);
        };
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == 0 {
            let placement = self.placement_dir.join(format!("{}.json", row.instance_id));
            let exact_nonce = std::fs::read(&placement)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| {
                    value
                        .get("start_nonce")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|nonce| Some(nonce.as_str()) == row.process_nonce.as_deref());
            return Ok(if exact_nonce {
                ProcessObservation::ExactLive
            } else {
                ProcessObservation::StaleLive
            });
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(ProcessObservation::Missing),
            _ => Ok(ProcessObservation::Unknown),
        }
    }

    fn reap_stale(&self, row: &InstanceRow) -> Result<()> {
        let placement = self.placement_dir.join(format!("{}.json", row.instance_id));
        let owned = std::fs::read(&placement)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .get("start_nonce")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|nonce| Some(nonce.as_str()) == row.process_nonce.as_deref());
        if owned {
            if let Some(pid) = row.process_id {
                if unsafe { libc::kill(pid as i32, 0) } == 0 {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
            }
        }
        Ok(())
    }

    fn adopted_child(
        &self,
        row: &InstanceRow,
    ) -> Result<Option<Box<dyn opencrab_process_supervisor::SupervisedChild>>> {
        Ok(row.process_id.map(|pid| {
            Box::new(opencrab_process_supervisor::AdoptedPidChild::new(pid))
                as Box<dyn opencrab_process_supervisor::SupervisedChild>
        }))
    }

    fn cleanup_artifacts(&self, row: &InstanceRow) -> Result<()> {
        for path in [
            self.placement_dir.join(format!("{}.json", row.instance_id)),
            self.placement_dir
                .join(format!("{}.control.sock", row.instance_id)),
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn control_socket(&self, row: &InstanceRow, _: &str) -> Option<PathBuf> {
        Some(
            self.placement_dir
                .join(format!("{}.control.sock", row.instance_id)),
        )
    }
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        start_nonce: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        let revision = row.core_revision.context("verified revision missing")?;
        std::fs::create_dir_all(&self.placement_dir)?;
        let path = self.placement_dir.join(format!("{}.json", row.instance_id));
        anyhow::ensure!(
            row.addresses.len() == 1,
            "Nostr instance requires exactly one address"
        );
        let value = serde_json::json!({
            "core_socket": self.core_socket,
            "nostaro_bin": self.nostaro_bin,
            "control_socket":self.control_socket(row,"").expect("production control socket"),
            "start_nonce":start_nonce,
            "instances": [{"instance_id":row.instance_id,"revision":revision,
                "address":row.addresses[0],"config_b64":row.config_b64}]
        });
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&value)?)?;
        std::fs::rename(temporary, &path)?;
        let secret = String::from_utf8(credential.to_vec()).context("credential is not UTF-8")?;
        Ok(Arc::new(
            opencrab_process_supervisor::ExternalProcessSpawner::with_secret_env(
                self.child_binary.clone(),
                path,
                secret,
                crate::secret::SECRET_ENV,
                "nostr-adapter",
                row.instance_id.clone(),
            ),
        ))
    }
}

pub async fn run(config: DaemonConfig) -> Result<()> {
    config.validate()?;
    let lock_path = config.database_path.with_extension("lock");
    let _lock = opencrab_process_supervisor::lock::StoreLock::acquire(&lock_path)
        .context("another Nostr daemon owns this store")?;
    let encoded_key = crate::secret::take_master_key().context("Nostr master key is required")?;
    let key = crate::secret_store::parse_master_key(&encoded_key)?;
    let mut raw = [0_u8; 32];
    raw.copy_from_slice(&key[..]);
    let store = NostrStore::open(&config.database_path)?;
    let client = opencrab_gate_client::admin::GateAdminClient::from_credential_file(
        config.gate_admin_socket.clone(),
        &config.gate_admin_credential,
    )?;
    let daemon = NostrDaemon::new(
        store,
        raw,
        Arc::new(UdsGateReconciler::new(client, "nostr")),
        Arc::new(ProductionFactory {
            child_binary: config.child_binary.clone(),
            core_socket: config.core_socket.clone(),
            placement_dir: config.placement_dir.clone(),
            nostaro_bin: config.nostaro_bin.clone(),
        }),
    )?;
    let admin_scope = Arc::new(config.admin_instance_ids.iter().cloned().collect());
    let _admin = crate::admin::spawn(
        config.admin_socket.clone(),
        admin_scope,
        daemon.store(),
        Arc::new(raw),
    );
    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(config.reconcile_millis));
    loop {
        interval.tick().await;
        daemon.reconcile_once().await?;
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
