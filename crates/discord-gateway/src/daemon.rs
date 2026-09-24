//! Discord-owned lifecycle daemon. Core is reached only through a generic control trait.

use crate::store::{DiscordStore, InstanceRow};
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
    pub gate_admin_socket: PathBuf,
    pub gate_admin_credential: PathBuf,
    pub core_socket: PathBuf,
    pub child_binary: PathBuf,
    pub placement_dir: PathBuf,
    #[serde(default)]
    pub attachment_spool_root: Option<PathBuf>,
    #[serde(default = "default_reconcile_millis")]
    pub reconcile_millis: u64,
}

fn default_reconcile_millis() -> u64 {
    1_000
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
    store: Arc<Mutex<DiscordStore>>,
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

pub struct DiscordDaemon<R, F> {
    store: Arc<Mutex<DiscordStore>>,
    key: Arc<[u8; 32]>,
    reconciler: Arc<R>,
    factory: Arc<F>,
    supervisors: Arc<ProcessSupervisorSet>,
}

impl<R, F> DiscordDaemon<R, F>
where
    R: GateReconciler + 'static,
    F: SpawnerFactory + 'static,
{
    pub fn new(
        store: DiscordStore,
        key: [u8; 32],
        reconciler: Arc<R>,
        factory: Arc<F>,
    ) -> Result<Self> {
        store.recover_startup_without_adoptable_processes(chrono::Utc::now().timestamp_millis())?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            key: Arc::new(key),
            reconciler,
            factory,
            supervisors: ProcessSupervisorSet::new(SupervisorConfig::daemon_owned()),
        })
    }

    pub fn store(&self) -> Arc<Mutex<DiscordStore>> {
        self.store.clone()
    }

    pub async fn reconcile_once(&self) -> Result<()> {
        let rows = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .list()?;
        for row in rows {
            match row.lifecycle_state {
                opencrab_process_supervisor::lifecycle::LifecycleState::Pending => {
                    self.supervisors.stop(&row.instance_id).await;
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
                            self.store.lock().unwrap().mark_error(
                                &row.instance_id,
                                row.desired_generation,
                                "reconciliation_failed",
                                chrono::Utc::now().timestamp_millis() + 1_000,
                            )?;
                            return Err(error);
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
                        self.start_ready(&row.instance_id).await?;
                    }
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Ready => {
                    self.start_ready(&row.instance_id).await?;
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Disabled
                | opencrab_process_supervisor::lifecycle::LifecycleState::Provisioning => {
                    self.supervisors.stop(&row.instance_id).await;
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Error => {
                    self.supervisors.stop(&row.instance_id).await;
                    if self.store.lock().unwrap().retry_due_error(
                        &row.instance_id,
                        row.desired_generation,
                        chrono::Utc::now().timestamp_millis(),
                    )? {
                        let recovered = self
                            .store
                            .lock()
                            .unwrap()
                            .get(&row.instance_id)?
                            .context("recovered instance missing")?;
                        if recovered.lifecycle_state
                            == opencrab_process_supervisor::lifecycle::LifecycleState::Ready
                        {
                            self.start_ready(&row.instance_id).await?;
                        }
                    }
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
                listener, store, id, generation, nonce,
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

async fn wait_for_child_readiness(
    listener: tokio::net::UnixListener,
    store: Arc<Mutex<DiscordStore>>,
    instance_id: String,
    generation: u64,
    expected_nonce: String,
) {
    use tokio::io::AsyncReadExt as _;
    let accepted =
        tokio::time::timeout(std::time::Duration::from_secs(30), listener.accept()).await;
    let Ok(Ok((mut stream, _))) = accepted else {
        return;
    };
    let mut bytes = Vec::new();
    if stream.read_to_end(&mut bytes).await.is_err() {
        return;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let Some(nonce) = value.get("nonce").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(pid) = value
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return;
    };
    if nonce != expected_nonce {
        return;
    }
    if let Ok(store) = store.lock() {
        let _ = store.mark_running(&instance_id, generation, pid, nonce);
    }
}

struct ProductionFactory {
    child_binary: PathBuf,
    core_socket: PathBuf,
    placement_dir: PathBuf,
    attachment_spool_root: Option<PathBuf>,
}

impl SpawnerFactory for ProductionFactory {
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
        let control_socket = self
            .control_socket(row, "")
            .expect("production control socket");
        let value = serde_json::json!({
            "core_socket": self.core_socket,
            "attachment_spool_root": self.attachment_spool_root,
            "control_socket": control_socket,
            "start_nonce": start_nonce,
            "instances": [{"instance_id":row.instance_id,"revision":revision,
                "addresses":row.addresses,"config_b64":row.config_b64}]
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
                crate::secret::TOKEN_ENV,
                "discord-adapter",
                row.instance_id.clone(),
            ),
        ))
    }
}

pub async fn run(config: DaemonConfig) -> Result<()> {
    config.validate()?;
    let lock_path = config.database_path.with_extension("lock");
    let _lock = opencrab_process_supervisor::lock::StoreLock::acquire(&lock_path)
        .context("another Discord daemon owns this store")?;
    let encoded_key = crate::secret::take_master_key().context("Discord master key is required")?;
    let key = crate::secret_store::parse_master_key(&encoded_key)?;
    let mut raw = [0_u8; 32];
    raw.copy_from_slice(&key[..]);
    let store = DiscordStore::open(&config.database_path)?;
    let client = opencrab_gate_client::admin::GateAdminClient::from_credential_file(
        config.gate_admin_socket.clone(),
        &config.gate_admin_credential,
    )?;
    let daemon = DiscordDaemon::new(
        store,
        raw,
        Arc::new(UdsGateReconciler::new(client, "discord")),
        Arc::new(ProductionFactory {
            child_binary: config.child_binary.clone(),
            core_socket: config.core_socket.clone(),
            placement_dir: config.placement_dir.clone(),
            attachment_spool_root: config.attachment_spool_root.clone(),
        }),
    )?;
    let _admin = crate::admin::spawn(config.admin_socket.clone(), daemon.store(), Arc::new(raw));
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
