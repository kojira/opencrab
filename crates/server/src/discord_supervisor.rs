//! discord-gateway 子プロセスの監視・自動再起動・後始末（DESIGN-DISCORD-GATE / #865）。
//!
//! server が spawn する外部gatewayは「1 process = 1 agent」。子のサイレント死を防ぐ。
//!
//! この module は 3 つを足す（core に Discord 語彙は増やさない・server の spawn 層で完結）:
//!
//! 1. **監視**: 子の終了を [`SupervisedChild::wait_exit`]（本番は `tokio` の `child.wait()`）で検知し、
//!    意図しない終了は fail-loud で ERROR（#857 `owner_warning` 流儀＝サイレント死の禁止）。
//! 2. **再起動**: 指数バックオフ（1s→2s→…→上限 60s・定着で reset）で自動再 spawn。連続失敗が
//!    閾値を超えたら警告を強めつつ **再試行は継続**（永久放置しない・ただし busy loop にもしない）。
//! 3. **後始末**: `shutdown` フラグ（[`tokio::sync::watch`]）が立ったら **再起動せず** 子を terminate
//!    （孤児プロセス防止）。本番の spawn は `kill_on_drop(true)` も併用し、タスク drop でも子を殺す。
//!
//! 再起動で子がcore UDSへ再接続すると、extgateのlive registryが再び稼働を示す。
//!
//! **秘密（bot token）**: 本番 spawner は token を **子の env のみ**へ注入し、親 env・argv・ログの
//! いずれにも出さない（`nostr-gateway` の watch 子と同じ流儀）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{error, info, warn};

/// 監視ポリシー。既定は「1s から倍々で 60s 上限・60s 生存で定着（reset）・連続 5 回で crash-loop 警告」。
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// 初回（および reset 直後）のバックオフ。
    pub base_delay: Duration,
    /// バックオフ上限（busy loop 防止のため頭打ちにする）。
    pub max_delay: Duration,
    /// 子がこの時間以上生きてから死んだら「再起動が定着した」とみなしバックオフ streak を畳む。
    pub reset_after: Duration,
    /// 連続でこの回数 quick death したら警告を強める（crash-loop）。`0` は無効。
    pub crash_loop_threshold: u32,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            reset_after: Duration::from_secs(60),
            crash_loop_threshold: 5,
        }
    }
}

/// 指数バックオフ。`consecutive`（1 起点）に対し `base * 2^(consecutive-1)` を `cap` で頭打ち。
///
/// `cap` を `base` 未満に設定しても `base` は下回らない。オーバーフローは飽和で吸う（`cap` に達したら
/// 早期に返すので、巨大な `consecutive` でもループは有界）。
pub fn backoff_delay(consecutive: u32, base: Duration, cap: Duration) -> Duration {
    let cap = cap.max(base);
    if consecutive <= 1 {
        return base.min(cap);
    }
    let mut delay = base;
    // consecutive-1 回だけ倍にする。cap に達したら即返す（有界）。
    for _ in 1..consecutive {
        delay = delay.saturating_mul(2);
        if delay >= cap {
            return cap;
        }
    }
    delay.min(cap)
}

/// 死亡後の連続失敗カウンタ更新。`uptime >= reset_after` なら定着とみなし streak を畳んで `1` に戻す。
/// それ以外は `prev + 1`（飽和加算）。
pub fn next_consecutive(prev: u32, uptime: Duration, reset_after: Duration) -> u32 {
    if uptime >= reset_after {
        1
    } else {
        prev.saturating_add(1)
    }
}

/// crash-loop（連続失敗が閾値以上）か。`threshold == 0` は常に false（無効）。
pub fn is_crash_loop(consecutive: u32, threshold: u32) -> bool {
    threshold > 0 && consecutive >= threshold
}

/// 子が **意図せず** 終了したときの fail-loud（#857 `owner_warning` 流儀）。鳴らしたら `true`。
///
/// **接続死のサイレント停止を潰すのが役目。** 外部gateway停止中は受信・配送とも停止する。
/// crash-loop時は原因の切り分け先（binary / placement / core UDS / credential）を残す。`error!`
/// なのは、子が死んでいる局面では Discord 経由通知も壊れうるため（ログなら落ちない）。
///
/// `outcome` には exit status の人間可読要約だけを渡す（**秘密を含めない**）。
pub fn escalate_child_exited(
    agent_id: &str,
    consecutive: u32,
    uptime_secs: u64,
    outcome: &str,
    next_delay_secs: u64,
    crash_loop_threshold: u32,
) -> bool {
    if is_crash_loop(consecutive, crash_loop_threshold) {
        error!(
            agent_id = %agent_id,
            consecutive,
            uptime_secs,
            outcome = %outcome,
            next_delay_secs,
            "gateway child has died {consecutive} times in a row (CRASH LOOP). Ingress and \
             delivery for this agent are DOWN with no legacy fallback. The supervisor keeps \
             retrying with capped backoff (next in {next_delay_secs}s). Check the gateway binary, \
             placement, core UDS socket, and child credential."
        );
    } else {
        error!(
            agent_id = %agent_id,
            consecutive,
            uptime_secs,
            outcome = %outcome,
            next_delay_secs,
            "gateway child exited WITHOUT a shutdown having been requested (was up \
             {uptime_secs}s). Ingress and delivery are down with no fallback until restart. \
             Auto-restarting in {next_delay_secs}s."
        );
    }
    true
}

/// spawn 自体が失敗したとき（binary が無い・権限が無い等）の fail-loud。鳴らしたら `true`。
pub fn escalate_spawn_failed(
    agent_id: &str,
    consecutive: u32,
    error: &str,
    next_delay_secs: u64,
) -> bool {
    error!(
        agent_id = %agent_id,
        consecutive,
        error = %error,
        next_delay_secs,
        "failed to (re)spawn the gateway child ({consecutive} attempts in a row). Ingress and \
         delivery for this agent are DOWN with no fallback. Retrying in {next_delay_secs}s. \
         Check the gateway binary path, permissions, and placement."
    );
    true
}

/// 監視対象の 1 子プロセス。本番は [`TokioChild`]、テストは fake で差し替える。
#[async_trait::async_trait]
pub trait SupervisedChild: Send {
    /// 子の終了を待ち、終了の人間可読な要約を返す（**秘密を含めない**）。
    async fn wait_exit(&mut self) -> String;
    /// 子を terminate して reap する（shutdown 時の後始末・ゾンビ防止）。
    async fn kill(&mut self);
    /// pid（ログ用）。
    fn pid(&self) -> Option<u32>;
}

/// 子を spawn する手段。本番は [`GatewayChildSpawner`]、テストは fake で差し替える。
#[async_trait::async_trait]
pub trait ChildSpawner: Send + Sync {
    /// 子を spawn する（placement.json は事前に書かれている前提・再起動でも同じ file を再 exec）。
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>>;
    /// どの agent / gateway か（ログ用）。
    fn agent_id(&self) -> &str;
    fn gateway_name(&self) -> &str {
        "gateway"
    }
}

struct SupervisedTask {
    shutdown: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

/// Owns one supervised external gateway task per agent and provides race-free replacement.
pub struct GatewaySupervisorSet {
    config: SupervisorConfig,
    tasks: tokio::sync::Mutex<HashMap<String, SupervisedTask>>,
    /// Serializes compound lifecycle operations so task replacement cannot race stop/shutdown.
    lifecycle: tokio::sync::Mutex<()>,
    closed: AtomicBool,
}

impl GatewaySupervisorSet {
    pub fn new(config: SupervisorConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            tasks: tokio::sync::Mutex::new(HashMap::new()),
            lifecycle: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        })
    }

    pub async fn start(&self, agent_id: &str, spawner: Arc<dyn ChildSpawner>) {
        let _lifecycle = self.lifecycle.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.stop_locked(agent_id).await;
        let (shutdown, receiver) = watch::channel(false);
        let config = self.config.clone();
        let join = tokio::spawn(supervise(spawner, config, receiver));
        self.tasks
            .lock()
            .await
            .insert(agent_id.to_string(), SupervisedTask { shutdown, join });
    }

    async fn stop_locked(&self, agent_id: &str) {
        let task = self.tasks.lock().await.remove(agent_id);
        if let Some(task) = task {
            let _ = task.shutdown.send(true);
            let _ = task.join.await;
        }
    }

    pub async fn stop(&self, agent_id: &str) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_locked(agent_id).await;
    }

    pub async fn shutdown_all(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        self.closed.store(true, Ordering::Release);
        let tasks: Vec<_> = self
            .tasks
            .lock()
            .await
            .drain()
            .map(|(_, task)| task)
            .collect();
        for task in &tasks {
            let _ = task.shutdown.send(true);
        }
        for task in tasks {
            let _ = task.join.await;
        }
    }
}

/// `tokio::process::Child` のラッパ。
pub struct TokioChild {
    child: tokio::process::Child,
    /// Spawned child is its own process-group leader. Descendants inherit this group.
    process_group: i32,
}

impl TokioChild {
    fn signal_group(&self, signal: i32) {
        // SAFETY: `process_group` is a positive pid captured directly after spawn. A negative
        // target asks kill(2) to signal that isolated process group, never the server's group.
        let rc = unsafe { libc::kill(-self.process_group, signal) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                warn!(%error, process_group = self.process_group, "failed to signal gateway process group");
            }
        }
    }
}

impl Drop for TokioChild {
    fn drop(&mut self) {
        // Backstop for task abort/runtime unwind: kill_on_drop covers only the direct child.
        // The isolated process group also contains transport helpers with the same credential.
        let _ = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
    }
}

#[async_trait::async_trait]
impl SupervisedChild for TokioChild {
    async fn wait_exit(&mut self) -> String {
        let outcome = match self.child.wait().await {
            Ok(status) => format!("{status}"),
            Err(e) => format!("wait() error: {e}"),
        };
        // If the gateway itself crashed, do not leave credential-bearing descendants behind.
        self.signal_group(libc::SIGKILL);
        outcome
    }

    async fn kill(&mut self) {
        // Give the whole isolated group a chance to exit, then force-clean every descendant.
        self.signal_group(libc::SIGTERM);
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            self.signal_group(libc::SIGKILL);
            let _ = self.child.wait().await;
        }
        self.signal_group(libc::SIGKILL);
    }

    fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

/// discord-gateway バイナリを exec する本番 spawner。
///
/// **bot token は子の env（`DISCORD_BOT_TOKEN`）のみ** に渡す（親 env も argv も汚さない・ログにも
/// 出さない）。`kill_on_drop(true)` で、監視タスクが drop されても子を確実に殺す（孤児防止の backstop）。
pub struct GatewayChildSpawner {
    bin: std::path::PathBuf,
    placement_path: std::path::PathBuf,
    /// 秘密。Debug 導出しない・ログに出さない。
    secret: String,
    secret_env: &'static str,
    gateway_name: &'static str,
    agent_id: String,
}

impl GatewayChildSpawner {
    pub fn new(
        bin: std::path::PathBuf,
        placement_path: std::path::PathBuf,
        bot_token: String,
        agent_id: String,
    ) -> Self {
        Self {
            bin,
            placement_path,
            secret: bot_token,
            secret_env: "DISCORD_BOT_TOKEN",
            gateway_name: "discord-gateway",
            agent_id,
        }
    }

    /// Nostr V3 gateway 用。復号済み鍵は子 env だけへ注入し、placement/argv へ載せない。
    pub fn new_nostr(
        bin: std::path::PathBuf,
        placement_path: std::path::PathBuf,
        secret_key: String,
        agent_id: String,
    ) -> Self {
        Self {
            bin,
            placement_path,
            secret: secret_key,
            secret_env: "NOSTARO_SECRET_KEY",
            gateway_name: "nostr-gateway",
            agent_id,
        }
    }
}

#[async_trait::async_trait]
impl ChildSpawner for GatewayChildSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg(&self.placement_path);
        cmd.env(self.secret_env, &self.secret);
        cmd.kill_on_drop(true);
        // External gateways may spawn transport helpers (for example `nostaro watch`). Keep
        // each gateway tree in an isolated group so stop/restart/shutdown cannot orphan them.
        std::os::unix::process::CommandExt::process_group(cmd.as_std_mut(), 0);
        let child = cmd.spawn()?;
        let process_group = child.id().ok_or_else(|| {
            std::io::Error::other("spawned gateway child did not expose a process id")
        })? as i32;
        Ok(Box::new(TokioChild {
            child,
            process_group,
        }))
    }

    fn agent_id(&self) -> &str {
        &self.agent_id
    }

    fn gateway_name(&self) -> &str {
        self.gateway_name
    }
}

/// 1 つの子を監視し続ける。`shutdown` が立つ（or 送信側 drop）まで戻らない。`tokio::spawn` して回す。
///
/// - 子が **意図せず** 死んだら fail-loud ERROR → バックオフ → 再 spawn。
/// - `shutdown` 中の終了・`shutdown` 要求は **再起動しない**（意図した停止・誤エスカレーションしない）。
/// - `shutdown` が立ったら生きている子を terminate（孤児防止）。
pub async fn supervise(
    spawner: Arc<dyn ChildSpawner>,
    cfg: SupervisorConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let agent_id = spawner.agent_id().to_string();
    let gateway_name = spawner.gateway_name().to_string();
    let mut consecutive: u32 = 0;

    loop {
        if *shutdown.borrow() {
            break;
        }

        // ---- spawn ----
        let mut child = match spawner.spawn().await {
            Ok(c) => {
                info!(
                    agent_id = %agent_id,
                    gateway = %gateway_name,
                    pid = ?c.pid(),
                    "gateway child started under supervision; credential injected by env"
                );
                c
            }
            Err(e) => {
                consecutive = consecutive.saturating_add(1);
                let delay = backoff_delay(consecutive, cfg.base_delay, cfg.max_delay);
                escalate_spawn_failed(&agent_id, consecutive, &e.to_string(), delay.as_secs());
                if sleep_or_shutdown(&mut shutdown, delay).await {
                    break;
                }
                continue;
            }
        };
        let started = Instant::now();

        // ---- 子の終了 or shutdown 要求を待つ ----
        tokio::select! {
            outcome = child.wait_exit() => {
                if *shutdown.borrow() {
                    // shutdown 中の終了は意図した停止。鳴らさず（誤エスカレーション防止）再起動もしない。
                    info!(
                        agent_id = %agent_id,
                        gateway = %gateway_name,
                        outcome = %outcome,
                        "gateway child exited during shutdown (expected; no restart)"
                    );
                    break;
                }
                let uptime = started.elapsed();
                consecutive = next_consecutive(consecutive, uptime, cfg.reset_after);
                let delay = backoff_delay(consecutive, cfg.base_delay, cfg.max_delay);
                escalate_child_exited(
                    &agent_id,
                    consecutive,
                    uptime.as_secs(),
                    &outcome,
                    delay.as_secs(),
                    cfg.crash_loop_threshold,
                );
                if sleep_or_shutdown(&mut shutdown, delay).await {
                    break;
                }
                // → loop 先頭へ戻って再 spawn。
            }
            _ = wait_for_shutdown(&mut shutdown) => {
                // 生きている子を terminate（孤児防止）。再起動はしない。
                info!(agent_id = %agent_id, gateway = %gateway_name, "terminating gateway child on shutdown");
                child.kill().await;
                break;
            }
        }
    }
}

/// `shutdown` が `true` になる（or 送信側が drop される）まで待つ。既に `true` なら即戻る。
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    // changed() は送信側 drop で Err を返す。drop も「プロセス終了」なので待機を終える。
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

/// `delay` 待つ。その間に shutdown が来たら `true`（→ 再起動せず break）、来なければ `false`。
async fn sleep_or_shutdown(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = wait_for_shutdown(shutdown) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::Notify;

    // ---- pure ロジック ----

    #[test]
    fn nostr_spawner_injects_only_the_nostr_secret_env() {
        let spawner = GatewayChildSpawner::new_nostr(
            "nostr-gateway".into(),
            "placement.json".into(),
            "not-a-real-secret".into(),
            "a1".into(),
        );
        assert_eq!(spawner.secret_env, "NOSTARO_SECRET_KEY");
        assert_eq!(spawner.gateway_name(), "nostr-gateway");
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
}
