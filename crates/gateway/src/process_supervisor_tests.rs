use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tokio::sync::Notify;

// ---- pure ロジック ----

#[test]
fn generic_spawner_injects_only_the_selected_secret_env() {
    let spawner = GatewayChildSpawner::with_secret_env(
        "example-gateway".into(),
        "placement.json".into(),
        "not-a-real-secret".into(),
        "EXAMPLE_GATEWAY_SECRET",
        "example-gateway",
        "a1".into(),
    );
    assert_eq!(spawner.secret_env, "EXAMPLE_GATEWAY_SECRET");
    assert_eq!(spawner.gateway_name(), "example-gateway");
    assert_eq!(spawner.agent_id(), "a1");
}

#[test]
fn backoff_doubles_and_caps() {
    let base = Duration::from_secs(1);
    let cap = Duration::from_secs(60);
    assert_eq!(backoff_delay(0, base, cap), Duration::from_secs(1)); // 0 も base 扱い
    assert_eq!(backoff_delay(1, base, cap), Duration::from_secs(1));
    assert_eq!(backoff_delay(2, base, cap), Duration::from_secs(2));
    assert_eq!(backoff_delay(3, base, cap), Duration::from_secs(4));
    assert_eq!(backoff_delay(4, base, cap), Duration::from_secs(8));
    assert_eq!(backoff_delay(5, base, cap), Duration::from_secs(16));
    assert_eq!(backoff_delay(6, base, cap), Duration::from_secs(32));
    // 7 回目で 64s → cap 60s に頭打ち。
    assert_eq!(backoff_delay(7, base, cap), Duration::from_secs(60));
    // 巨大な連続失敗でも cap で頭打ち（有界・busy loop にしない）。
    assert_eq!(backoff_delay(1_000_000, base, cap), Duration::from_secs(60));
}

#[test]
fn backoff_never_below_base_even_if_cap_is_smaller() {
    let base = Duration::from_secs(5);
    let cap = Duration::from_secs(1);
    assert_eq!(backoff_delay(1, base, cap), Duration::from_secs(5));
    assert_eq!(backoff_delay(3, base, cap), Duration::from_secs(5));
}

#[test]
fn consecutive_resets_when_child_stuck_long_enough() {
    let reset = Duration::from_secs(60);
    // quick death（reset 未満）は streak を伸ばす。
    assert_eq!(next_consecutive(0, Duration::from_secs(1), reset), 1);
    assert_eq!(next_consecutive(4, Duration::from_secs(2), reset), 5);
    // 定着（reset 以上生存）してから死んだら 1 に畳む。
    assert_eq!(next_consecutive(9, Duration::from_secs(60), reset), 1);
    assert_eq!(next_consecutive(9, Duration::from_secs(120), reset), 1);
}

#[test]
fn crash_loop_predicate() {
    assert!(!is_crash_loop(4, 5));
    assert!(is_crash_loop(5, 5));
    assert!(is_crash_loop(9, 5));
    // 閾値 0 は無効（常に false）。
    assert!(!is_crash_loop(100, 0));
}

#[test]
fn escalations_fire() {
    // fail-loud は必ず鳴る（サイレント死を作らない）。crash-loop 分岐も両方通す。
    assert!(escalate_child_exited("a1", 1, 3, "exit status: 1", 1, 5));
    assert!(escalate_child_exited("a1", 6, 0, "signal: 9", 60, 5));
    assert!(escalate_spawn_failed("a1", 2, "No such file", 2));
}

// ---- loop 配線（fake spawner / fake child で実プロセス無しに検証）----

struct FakeChild {
    die: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SupervisedChild for FakeChild {
    async fn wait_exit(&mut self) -> String {
        self.die.notified().await;
        "fake exit".to_string()
    }
    async fn kill(&mut self) {
        self.kills.fetch_add(1, Ordering::SeqCst);
        // kill されたら wait_exit も解ける（本番の wait() 同様）。
        self.die.notify_one();
    }
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct HangingChild {
    kill_started: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Drop for HangingChild {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl SupervisedChild for HangingChild {
    async fn wait_exit(&mut self) -> String {
        std::future::pending().await
    }

    async fn kill(&mut self) {
        self.kill_started.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }

    fn pid(&self) -> Option<u32> {
        Some(4243)
    }
}

struct HangingSpawner {
    spawned: Arc<Notify>,
    kill_started: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ChildSpawner for HangingSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        self.spawned.notify_one();
        Ok(Box::new(HangingChild {
            kill_started: self.kill_started.clone(),
            drops: self.drops.clone(),
        }))
    }

    fn agent_id(&self) -> &str {
        "hanging"
    }
}

struct FakeSpawner {
    agent_id: String,
    spawns: Arc<AtomicUsize>,
    kills: Arc<AtomicUsize>,
    /// spawn するたびに、その子の die-notify を積む（テストが特定の子を殺せるように）。
    dies: Arc<Mutex<Vec<Arc<Notify>>>>,
}

#[async_trait::async_trait]
impl ChildSpawner for FakeSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let die = Arc::new(Notify::new());
        self.dies.lock().unwrap().push(die.clone());
        Ok(Box::new(FakeChild {
            die,
            kills: self.kills.clone(),
        }))
    }
    fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

fn fast_cfg() -> SupervisorConfig {
    // バックオフを極小にして再起動を即時にする（reset_after は長め＝quick death を維持）。
    SupervisorConfig {
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        reset_after: Duration::from_secs(3600),
        crash_loop_threshold: 5,
    }
}

async fn wait_until<F: Fn() -> bool>(pred: F) -> bool {
    for _ in 0..400 {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    pred()
}

/// 異常終了（shutdown 無し）→ 検知して再 spawn される。
#[tokio::test]
async fn abnormal_exit_triggers_respawn() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let kills = Arc::new(AtomicUsize::new(0));
    let dies: Arc<Mutex<Vec<Arc<Notify>>>> = Arc::default();
    let spawner = Arc::new(FakeSpawner {
        agent_id: "a1".into(),
        spawns: spawns.clone(),
        kills: kills.clone(),
        dies: dies.clone(),
    });
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(spawner, fast_cfg(), rx));

    // 初回 spawn。
    assert!(wait_until(|| spawns.load(Ordering::SeqCst) >= 1).await);
    // 1 匹目を殺す（shutdown ではない異常終了）→ supervisor が再 spawn するはず。
    dies.lock().unwrap()[0].notify_one();
    assert!(
        wait_until(|| spawns.load(Ordering::SeqCst) >= 2).await,
        "異常終了後に再 spawn されない（spawns={}）",
        spawns.load(Ordering::SeqCst)
    );

    // 片付け: shutdown で終わらせる。
    tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// shutdown 要求 → 再起動せず子を terminate（kill が呼ばれ、以後 spawn しない）。
#[tokio::test]
async fn shutdown_terminates_child_and_does_not_restart() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let kills = Arc::new(AtomicUsize::new(0));
    let dies: Arc<Mutex<Vec<Arc<Notify>>>> = Arc::default();
    let spawner = Arc::new(FakeSpawner {
        agent_id: "a1".into(),
        spawns: spawns.clone(),
        kills: kills.clone(),
        dies: dies.clone(),
    });
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(supervise(spawner, fast_cfg(), rx));

    assert!(wait_until(|| spawns.load(Ordering::SeqCst) >= 1).await);
    let spawns_at_shutdown = spawns.load(Ordering::SeqCst);

    // shutdown → 生きている子を terminate。
    tx.send(true).unwrap();
    // supervise が戻る（再起動ループに入らず break）。
    let joined = tokio::time::timeout(Duration::from_secs(5), task).await;
    assert!(joined.is_ok(), "shutdown で supervise が戻らない");

    assert_eq!(
        kills.load(Ordering::SeqCst),
        1,
        "子が terminate されていない"
    );
    assert_eq!(
        spawns.load(Ordering::SeqCst),
        spawns_at_shutdown,
        "shutdown 後に再 spawn してはいけない"
    );
}

#[tokio::test]
async fn concurrent_replacement_leaves_one_owned_child() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let kills = Arc::new(AtomicUsize::new(0));
    let dies: Arc<Mutex<Vec<Arc<Notify>>>> = Arc::default();
    let spawner = Arc::new(FakeSpawner {
        agent_id: "a1".into(),
        spawns: spawns.clone(),
        kills: kills.clone(),
        dies,
    });
    let set = GatewaySupervisorSet::new(fast_cfg());
    tokio::join!(set.start("a1", spawner.clone()), set.start("a1", spawner));
    assert!(wait_until(|| spawns.load(Ordering::SeqCst) >= 1).await);
    set.shutdown_all().await;
    assert_eq!(
        kills.load(Ordering::SeqCst),
        spawns.load(Ordering::SeqCst),
        "every spawned child must remain owned and terminated"
    );
}

#[tokio::test]
async fn shutdown_all_bounds_hung_supervisor_join_and_aborts_owner() {
    let spawned = Arc::new(Notify::new());
    let kill_started = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let spawner = Arc::new(HangingSpawner {
        spawned: spawned.clone(),
        kill_started: kill_started.clone(),
        drops: drops.clone(),
    });
    let set = GatewaySupervisorSet::new(fast_cfg());
    set.start("hanging", spawner).await;
    spawned.notified().await;

    let started = tokio::time::Instant::now();
    set.shutdown_all().await;

    assert_eq!(kill_started.load(Ordering::SeqCst), 1);
    assert!(started.elapsed() >= SUPERVISOR_JOIN_TIMEOUT);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_replacement_does_not_leave_an_untracked_owner() {
    let spawned = Arc::new(Notify::new());
    let kill_started = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let set = GatewaySupervisorSet::new(fast_cfg());
    set.start(
        "hanging",
        Arc::new(HangingSpawner {
            spawned: spawned.clone(),
            kill_started: kill_started.clone(),
            drops: drops.clone(),
        }),
    )
    .await;
    spawned.notified().await;

    let replacement_spawns = Arc::new(AtomicUsize::new(0));
    let replacement = Arc::new(FakeSpawner {
        agent_id: "hanging".into(),
        spawns: replacement_spawns.clone(),
        kills: Arc::new(AtomicUsize::new(0)),
        dies: Arc::default(),
    });
    let replacing_set = set.clone();
    let replacing = tokio::spawn(async move {
        replacing_set.start("hanging", replacement).await;
    });
    assert!(wait_until(|| kill_started.load(Ordering::SeqCst) == 1).await);

    replacing.abort();
    let _ = replacing.await;
    assert!(wait_until(|| drops.load(Ordering::SeqCst) == 1).await);
    set.shutdown_all().await;
    assert_eq!(replacement_spawns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shutdown_is_terminal_against_concurrent_or_later_start() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let spawner = Arc::new(FakeSpawner {
        agent_id: "a1".into(),
        spawns: spawns.clone(),
        kills: Arc::new(AtomicUsize::new(0)),
        dies: Arc::default(),
    });
    let set = GatewaySupervisorSet::new(fast_cfg());
    set.shutdown_all().await;
    set.start("a1", spawner).await;
    tokio::task::yield_now().await;
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
}

/// 起動前に既に shutdown なら 1 度も spawn しない。
#[tokio::test]
async fn already_shutdown_never_spawns() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let kills = Arc::new(AtomicUsize::new(0));
    let dies: Arc<Mutex<Vec<Arc<Notify>>>> = Arc::default();
    let spawner = Arc::new(FakeSpawner {
        agent_id: "a1".into(),
        spawns: spawns.clone(),
        kills: kills.clone(),
        dies: dies.clone(),
    });
    let (tx, rx) = watch::channel(true); // 最初から shutdown。
    let _ = tx; // 送信側は保持だけ。
    let task = tokio::spawn(supervise(spawner, fast_cfg(), rx));
    let joined = tokio::time::timeout(Duration::from_secs(5), task).await;
    assert!(joined.is_ok(), "既 shutdown で即戻らない");
    assert_eq!(spawns.load(Ordering::SeqCst), 0, "shutdown 中に spawn した");
}
