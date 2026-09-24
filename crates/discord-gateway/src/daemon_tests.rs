use super::*;
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct FakeGate;
#[async_trait]
impl GateReconciler for FakeGate {
    async fn reconcile(
        &self,
        desired: &InstanceRow,
        subject_grant: Option<&str>,
    ) -> Result<VerifiedInstance> {
        assert_eq!(subject_grant, Some("grant-secret"));
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
    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("core server is stopped")
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
