use super::*;
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct FakeGate;
#[async_trait]
impl GateReconciler for FakeGate {
    async fn observe(
        &self,
        desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(if desired.enabled {
            opencrab_process_supervisor::lifecycle::CoreObservation::ExactEnabled
        } else {
            opencrab_process_supervisor::lifecycle::CoreObservation::ExactDisabled
        })
    }

    async fn reconcile(
        &self,
        desired: &InstanceRow,
        subject_grant: Option<&str>,
    ) -> Result<VerifiedInstance> {
        assert!(subject_grant.is_none() || subject_grant == Some("grant-secret"));
        Ok(VerifiedInstance {
            revision: 4,
            digest: "digest".into(),
            bindings: desired.addresses.clone(),
            enabled: desired.enabled,
        })
    }
}

struct OfflineGate;
#[async_trait]
impl GateReconciler for OfflineGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(opencrab_process_supervisor::lifecycle::CoreObservation::Unavailable)
    }

    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("core server is stopped")
    }
}

struct FailingGate;
#[async_trait]
impl GateReconciler for FailingGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(opencrab_process_supervisor::lifecycle::CoreObservation::Mismatch)
    }

    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("core unavailable")
    }
}

struct RecoveryGate(opencrab_process_supervisor::lifecycle::CoreObservation);
#[async_trait]
impl GateReconciler for RecoveryGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(self.0)
    }

    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("reconciliation not expected during startup matrix")
    }
}

struct RecoveryFactory {
    observation: opencrab_process_supervisor::lifecycle::ProcessObservation,
    reaped: Arc<AtomicUsize>,
    cleaned: Arc<AtomicUsize>,
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
impl SpawnerFactory for RecoveryFactory {
    fn observe_process(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
        Ok(self.observation)
    }
    fn reap_stale(&self, _: &InstanceRow) -> Result<()> {
        self.reaped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn cleanup_artifacts(&self, _: &InstanceRow) -> Result<()> {
        self.cleaned.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn adopted_child(&self, _: &InstanceRow) -> Result<Option<Box<dyn SupervisedChild>>> {
        Ok(Some(Box::new(FakeChild {
            exit: self.exit.clone(),
            kills: self.kills.clone(),
        })))
    }
    fn for_instance(
        &self,
        _: &InstanceRow,
        _: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        anyhow::bail!("spawn not expected during startup matrix")
    }
}

struct FakeChild {
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
#[async_trait]
impl SupervisedChild for FakeChild {
    async fn wait_exit(&mut self) -> String {
        self.exit.notified().await;
        "crash".into()
    }
    async fn kill(&mut self) {
        self.kills.fetch_add(1, Ordering::SeqCst);
    }
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct FakeSpawner {
    target: String,
    started: Arc<AtomicUsize>,
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
#[async_trait]
impl ChildSpawner for FakeSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeChild {
            exit: self.exit.clone(),
            kills: self.kills.clone(),
        }))
    }
    fn target_id(&self) -> &str {
        &self.target
    }
    fn service_name(&self) -> &str {
        "discord-adapter"
    }
}

struct FakeFactory {
    started: Arc<AtomicUsize>,
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
impl SpawnerFactory for FakeFactory {
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        assert_eq!(&*credential, b"token-secret");
        Ok(Arc::new(FakeSpawner {
            target: row.instance_id.clone(),
            started: self.started.clone(),
            exit: self.exit.clone(),
            kills: self.kills.clone(),
        }))
    }
}

fn daemon(
    enabled: bool,
) -> (
    tempfile::TempDir,
    DiscordDaemon<FakeGate, FakeFactory>,
    Arc<AtomicUsize>,
    Arc<Notify>,
) {
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &["opaque-address".into()],
            "token-secret",
            Some("grant-secret"),
            enabled,
            &[9; 32],
        )
        .unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let exit = Arc::new(Notify::new());
    let factory = Arc::new(FakeFactory {
        started: started.clone(),
        exit: exit.clone(),
        kills: Arc::new(AtomicUsize::new(0)),
    });
    (
        temp,
        DiscordDaemon::new(store, [9; 32], Arc::new(FakeGate), factory).unwrap(),
        started,
        exit,
    )
}

#[tokio::test]
async fn s5_discord_daemon_runs_and_crash_state_persists_while_server_is_stopped() {
    let (_temp, daemon, started, exit) = daemon(true);
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 1);
    let store = daemon.store();
    let ready = store
        .lock()
        .unwrap()
        .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .unwrap()
        .unwrap();
    assert_eq!(
        ready.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Ready
    );
    let nonce = ready.process_nonce.clone().unwrap();
    assert!(daemon
        .confirm_child_ready(&ready.instance_id, ready.desired_generation, 4242, &nonce)
        .unwrap());
    exit.notify_one();
    for _ in 0..100 {
        if store
            .lock()
            .unwrap()
            .get(&ready.instance_id)
            .unwrap()
            .unwrap()
            .lifecycle_state
            == opencrab_process_supervisor::lifecycle::LifecycleState::Error
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let crashed = store
        .lock()
        .unwrap()
        .get(&ready.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        crashed.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Error
    );
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "durable daemon, not utility, owns retry"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
    daemon.reconcile_once().await.unwrap();
    assert_eq!(
        daemon
            .store()
            .lock()
            .unwrap()
            .get(&ready.instance_id)
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Pending
    );
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 2);
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_restart_starts_persisted_ready_child_while_server_is_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("owner.db");
    let store = DiscordStore::open(&path).unwrap();
    let generation = store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &["opaque-address".into()],
            "token-secret",
            Some("grant-secret"),
            true,
            &[9; 32],
        )
        .unwrap();
    assert!(store
        .mark_provisioning("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", generation)
        .unwrap());
    assert!(store
        .mark_verified(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            generation,
            4,
            "digest",
            &["binding".into()],
            true,
        )
        .unwrap());
    drop(store);

    let started = Arc::new(AtomicUsize::new(0));
    let daemon = DiscordDaemon::new(
        DiscordStore::open(&path).unwrap(),
        [9; 32],
        Arc::new(OfflineGate),
        Arc::new(FakeFactory {
            started: started.clone(),
            exit: Arc::new(Notify::new()),
            kills: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .unwrap();
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 1);
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_daemon_startup_recovery_matrix_uses_process_and_core_observations() {
    use opencrab_process_supervisor::lifecycle::{
        CoreObservation, LifecycleState, ProcessObservation,
    };
    for state in [
        LifecycleState::Disabled,
        LifecycleState::Pending,
        LifecycleState::Provisioning,
        LifecycleState::Ready,
        LifecycleState::Running,
        LifecycleState::Error,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
        let enabled = state != LifecycleState::Disabled;
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let generation = store
            .upsert_desired(
                id,
                "agent",
                7,
                "e30=",
                &[],
                "token",
                None,
                enabled,
                &[9; 32],
            )
            .unwrap();
        if !matches!(state, LifecycleState::Pending) {
            store.mark_provisioning(id, generation).unwrap();
        }
        if matches!(
            state,
            LifecycleState::Disabled
                | LifecycleState::Ready
                | LifecycleState::Running
                | LifecycleState::Error
        ) {
            store
                .mark_verified(id, generation, 1, "digest", &[], enabled)
                .unwrap();
        }
        if matches!(state, LifecycleState::Running) {
            store.record_started(id, generation, 4242, "nonce").unwrap();
            store.mark_running(id, generation, 4242, "nonce").unwrap();
        }
        if matches!(state, LifecycleState::Error) {
            store
                .mark_error(
                    id,
                    generation,
                    "failed",
                    chrono::Utc::now().timestamp_millis() + 60_000,
                )
                .unwrap();
        }
        let reaped = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let daemon = DiscordDaemon::new(
            store,
            [9; 32],
            Arc::new(RecoveryGate(if enabled {
                CoreObservation::ExactEnabled
            } else {
                CoreObservation::ExactDisabled
            })),
            Arc::new(RecoveryFactory {
                observation: if state == LifecycleState::Running {
                    ProcessObservation::ExactLive
                } else {
                    ProcessObservation::Missing
                },
                reaped: reaped.clone(),
                cleaned: cleaned.clone(),
                exit: Arc::new(Notify::new()),
                kills: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        daemon.recover_startup().await.unwrap();
        let recovered = daemon.store().lock().unwrap().get(id).unwrap().unwrap();
        let expected = match state {
            LifecycleState::Provisioning => LifecycleState::Pending,
            other => other,
        };
        assert_eq!(recovered.lifecycle_state, expected, "{state:?}");
        assert_eq!(
            reaped.load(Ordering::SeqCst),
            usize::from(state != LifecycleState::Running),
            "{state:?}"
        );
        assert_eq!(
            cleaned.load(Ordering::SeqCst),
            usize::from(state != LifecycleState::Running),
            "{state:?}"
        );
        daemon.shutdown().await;
    }
}

#[tokio::test]
async fn s5_discord_running_loss_and_stale_process_are_reaped_to_durable_error() {
    use opencrab_process_supervisor::lifecycle::{
        CoreObservation, LifecycleState, ProcessObservation,
    };
    for observation in [ProcessObservation::Missing, ProcessObservation::StaleLive] {
        let temp = tempfile::tempdir().unwrap();
        let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let generation = store
            .upsert_desired(id, "agent", 7, "e30=", &[], "token", None, true, &[9; 32])
            .unwrap();
        store.mark_provisioning(id, generation).unwrap();
        store
            .mark_verified(id, generation, 1, "digest", &[], true)
            .unwrap();
        store.record_started(id, generation, 4242, "nonce").unwrap();
        store.mark_running(id, generation, 4242, "nonce").unwrap();
        let reaped = Arc::new(AtomicUsize::new(0));
        let daemon = DiscordDaemon::new(
            store,
            [9; 32],
            Arc::new(RecoveryGate(CoreObservation::ExactEnabled)),
            Arc::new(RecoveryFactory {
                observation,
                reaped: reaped.clone(),
                cleaned: Arc::new(AtomicUsize::new(0)),
                exit: Arc::new(Notify::new()),
                kills: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        daemon.recover_startup().await.unwrap();
        assert_eq!(
            daemon
                .store()
                .lock()
                .unwrap()
                .get(id)
                .unwrap()
                .unwrap()
                .lifecycle_state,
            LifecycleState::Error
        );
        assert_eq!(reaped.load(Ordering::SeqCst), 1);
        daemon.shutdown().await;
    }
}

#[tokio::test]
async fn s5_discord_reconciliation_failure_is_durable_and_does_not_kill_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &[],
            "token-secret",
            Some("grant-secret"),
            true,
            &[9; 32],
        )
        .unwrap();
    let daemon = DiscordDaemon::new(
        store,
        [9; 32],
        Arc::new(FailingGate),
        Arc::new(FakeFactory {
            started: Arc::new(AtomicUsize::new(0)),
            exit: Arc::new(Notify::new()),
            kills: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .unwrap();
    assert!(daemon.reconcile_once().await.is_ok());
    assert_eq!(
        daemon
            .store()
            .lock()
            .unwrap()
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Error
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_malformed_readiness_reaps_child_and_persists_retry() {
    use tokio::io::AsyncWriteExt as _;
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let generation = store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &[],
            "token-secret",
            None,
            true,
            &[9; 32],
        )
        .unwrap();
    store
        .mark_provisioning("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", generation)
        .unwrap();
    store
        .mark_verified(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            generation,
            1,
            "digest",
            &[],
            true,
        )
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let socket = temp.path().join("ready.sock");
    let listener = prepare_readiness_listener(&socket).unwrap();
    let supervisors = ProcessSupervisorSet::new(SupervisorConfig::daemon_owned());
    let factory = Arc::new(FakeFactory {
        started: Arc::new(AtomicUsize::new(0)),
        exit: Arc::new(Notify::new()),
        kills: Arc::new(AtomicUsize::new(0)),
    });
    let waiter = tokio::spawn(wait_for_child_readiness(
        listener,
        store.clone(),
        supervisors,
        factory,
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        generation,
        "expected".into(),
    ));
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream.write_all(b"{}").await.unwrap();
    stream.shutdown().await.unwrap();
    waiter.await.unwrap();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Error
    );
}

#[tokio::test]
async fn s5_discord_disabled_recovery_removes_daemon_owned_placement_and_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let placement_dir = temp.path().join("placements");
    std::fs::create_dir_all(&placement_dir).unwrap();
    let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let placement = placement_dir.join(format!("{instance_id}.json"));
    let control = placement_dir.join(format!("{instance_id}.control.sock"));
    std::fs::write(&placement, b"owned").unwrap();
    std::fs::write(&control, b"owned").unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let generation = store
        .upsert_desired(
            instance_id,
            "agent",
            7,
            "e30=",
            &[],
            "token",
            None,
            false,
            &[9; 32],
        )
        .unwrap();
    store.mark_provisioning(instance_id, generation).unwrap();
    store
        .mark_verified(instance_id, generation, 1, "digest", &[], false)
        .unwrap();
    let daemon = DiscordDaemon::new(
        store,
        [9; 32],
        Arc::new(OfflineGate),
        Arc::new(ProductionFactory {
            child_binary: "/bin/true".into(),
            core_socket: temp.path().join("core.sock"),
            placement_dir,
            attachment_spool_root: None,
        }),
    )
    .unwrap();
    daemon.reconcile_once().await.unwrap();
    assert!(!placement.exists());
    assert!(!control.exists());
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_disabled_and_nonready_instances_never_spawn() {
    let (_temp, daemon, started, _) = daemon(false);
    daemon.reconcile_once().await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(started.load(Ordering::SeqCst), 0);
    let row = daemon
        .store()
        .lock()
        .unwrap()
        .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .unwrap()
        .unwrap();
    assert_eq!(
        row.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Disabled
    );
    daemon.shutdown().await;
}

#[test]
fn s5_discord_runtime_config_rejects_core_and_legacy_database_paths() {
    let value = serde_json::json!({
        "database_path":"/tmp/discord.db","admin_socket":"/tmp/admin.sock",
        "gate_admin_socket":"/tmp/gate-admin.sock","gate_admin_credential":"/tmp/token",
        "core_socket":"/tmp/runtime.sock","child_binary":"/bin/true","placement_dir":"/tmp/place",
        "core_database_path":"/tmp/core.db"
    });
    assert!(serde_json::from_value::<DaemonConfig>(value).is_err());
    let legacy = serde_json::json!({
        "database_path":"/tmp/discord.db","admin_socket":"/tmp/admin.sock",
        "gate_admin_socket":"/tmp/gate-admin.sock","gate_admin_credential":"/tmp/token",
        "core_socket":"/tmp/runtime.sock","child_binary":"/bin/true","placement_dir":"/tmp/place",
        "legacy_database_path":"/tmp/core.db"
    });
    assert!(serde_json::from_value::<DaemonConfig>(legacy).is_err());
}
