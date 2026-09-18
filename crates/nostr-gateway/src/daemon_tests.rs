use super::*;
use opencrab_gateway::process_supervisor::{ChildSpawner, SupervisedChild};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::Notify;

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

struct TrackingChild {
    kills: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SupervisedChild for TrackingChild {
    async fn wait_exit(&mut self) -> String {
        std::future::pending().await
    }

    async fn kill(&mut self) {
        self.kills.fetch_add(1, Ordering::SeqCst);
    }

    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct TrackingSpawner {
    spawned: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChildSpawner for TrackingSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        self.spawned.notify_one();
        Ok(Box::new(TrackingChild {
            kills: self.kills.clone(),
        }))
    }

    fn agent_id(&self) -> &str {
        "identity-a"
    }
}

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
    // Both signal streams are registered before any supervised process is started.
    let signal = shutdown_signal().unwrap();
    let supervisors = start_process_tree(script, &pid_file).await;
    std::fs::write(ready_file, b"ready").unwrap();
    shutdown_supervisors_on_exit(supervisors, std::future::pending(), signal)
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

    shutdown_supervisors_on_exit(supervisors, async { Ok(()) }, std::future::pending())
        .await
        .unwrap();
    assert_processes_gone(replacement_pids).await;
    drop(process_guard);
}

#[tokio::test]
async fn shutdown_interrupts_blocked_reconciliation_and_cleans_its_supervisor() {
    let supervisors = GatewaySupervisorSet::new(SupervisorConfig::default());
    let spawned = Arc::new(Notify::new());
    let kills = Arc::new(AtomicUsize::new(0));
    let spawner = Arc::new(TrackingSpawner {
        spawned: spawned.clone(),
        kills: kills.clone(),
    });
    let work_supervisors = supervisors.clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(shutdown_supervisors_on_exit(
        supervisors,
        async move {
            work_supervisors.start("identity-a", spawner).await;
            std::future::pending().await
        },
        async move {
            shutdown_rx
                .await
                .map_err(|_| anyhow::anyhow!("shutdown sender dropped"))
        },
    ));

    spawned.notified().await;
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("shutdown must interrupt blocked reconciliation")
        .unwrap()
        .unwrap();
    assert_eq!(kills.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn work_error_is_preserved_after_supervisor_cleanup() {
    let supervisors = GatewaySupervisorSet::new(SupervisorConfig::default());
    let spawned = Arc::new(Notify::new());
    let kills = Arc::new(AtomicUsize::new(0));
    let spawner = Arc::new(TrackingSpawner {
        spawned: spawned.clone(),
        kills: kills.clone(),
    });
    let work_supervisors = supervisors.clone();

    let error = shutdown_supervisors_on_exit(
        supervisors,
        async move {
            work_supervisors.start("identity-a", spawner).await;
            spawned.notified().await;
            anyhow::bail!("original reconciliation error")
        },
        std::future::pending(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "original reconciliation error");
    assert_eq!(kills.load(Ordering::SeqCst), 1);
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
