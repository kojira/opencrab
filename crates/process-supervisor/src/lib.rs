//! Daemon-owned external child monitoring, restart, and cleanup.
//!
//! The caller is the sole lifecycle authority for each target. This utility has no platform,
//! server, configuration-schema, or placement knowledge.
//!
//! The utility provides three generic process guarantees:
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
//! **Secrets**: the production spawner injects decrypted bytes only into the selected child
//! environment and never into the parent environment, argv, config file, or logs.

pub mod lifecycle;
pub mod lock;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{error, info, warn};

const TERMINATE_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const FORCE_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(test))]
const SUPERVISOR_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SUPERVISOR_JOIN_TIMEOUT: Duration = Duration::from_millis(50);
const ABORTED_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// `false` when a durable daemon saga, rather than this utility, owns retry timing.
    pub restart_on_exit: bool,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            reset_after: Duration::from_secs(60),
            crash_loop_threshold: 5,
            restart_on_exit: true,
        }
    }
}

impl SupervisorConfig {
    pub fn daemon_owned() -> Self {
        Self {
            restart_on_exit: false,
            ..Self::default()
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
/// Prevent silent child loss. During a crash loop, retain a generic diagnostic pointing to the
/// executable, config, service endpoint, or credential without exposing their contents.
///
/// `outcome` には exit status の人間可読要約だけを渡す（**秘密を含めない**）。
pub fn escalate_child_exited(
    target_id: &str,
    consecutive: u32,
    uptime_secs: u64,
    outcome: &str,
    next_delay_secs: u64,
    crash_loop_threshold: u32,
) -> bool {
    if is_crash_loop(consecutive, crash_loop_threshold) {
        error!(
            target_id = %target_id,
            consecutive,
            uptime_secs,
            outcome = %outcome,
            next_delay_secs,
            "external child has died {consecutive} times in a row (CRASH LOOP). Ingress and \
             delivery for this agent are DOWN with no legacy fallback. The supervisor keeps \
             retrying with capped backoff (next in {next_delay_secs}s). Check the external binary, \
             config, core UDS socket, and child credential."
        );
    } else {
        error!(
            target_id = %target_id,
            consecutive,
            uptime_secs,
            outcome = %outcome,
            next_delay_secs,
            "external child exited WITHOUT a shutdown having been requested (was up \
             {uptime_secs}s). Ingress and delivery are down with no fallback until restart. \
             Auto-restarting in {next_delay_secs}s."
        );
    }
    true
}

/// spawn 自体が失敗したとき（binary が無い・権限が無い等）の fail-loud。鳴らしたら `true`。
pub fn escalate_spawn_failed(
    target_id: &str,
    consecutive: u32,
    error: &str,
    next_delay_secs: u64,
) -> bool {
    error!(
        target_id = %target_id,
        consecutive,
        error = %error,
        next_delay_secs,
        "failed to (re)spawn the external child ({consecutive} attempts in a row). Ingress and \
         delivery for this agent are DOWN with no fallback. Retrying in {next_delay_secs}s. \
         Check the external binary path, permissions, and config."
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

/// Monitor for a live process inherited across daemon restart.
pub struct AdoptedPidChild {
    pid: u32,
}

impl AdoptedPidChild {
    pub fn new(pid: u32) -> Self {
        Self { pid }
    }

    fn is_live(&self) -> bool {
        (unsafe { libc::kill(self.pid as i32, 0) }) == 0
    }
}

#[async_trait::async_trait]
impl SupervisedChild for AdoptedPidChild {
    async fn wait_exit(&mut self) -> String {
        while self.is_live() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        "adopted_process_exit".into()
    }

    async fn kill(&mut self) {
        if !self.is_live() {
            return;
        }
        unsafe { libc::kill(self.pid as i32, libc::SIGTERM) };
        for _ in 0..30 {
            if !self.is_live() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
    }

    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }
}

/// 子を spawn する手段。本番は [`ExternalProcessSpawner`]、テストは fake で差し替える。
#[async_trait::async_trait]
pub trait ChildSpawner: Send + Sync {
    /// 子を spawn する（config.json は事前に書かれている前提・再起動でも同じ file を再 exec）。
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>>;
    /// Stable target identifier used only for lifecycle ownership and redacted logs.
    fn target_id(&self) -> &str;
    fn service_name(&self) -> &str {
        "external-service"
    }
}

#[async_trait::async_trait]
pub trait ProcessObserver: Send + Sync {
    async fn spawned(&self, _target_id: &str, _pid: Option<u32>) {}
    async fn spawn_failed(&self, _target_id: &str) {}
    async fn exited(&self, _target_id: &str, _summary: &str) {}
}

struct NoopObserver;
#[async_trait::async_trait]
impl ProcessObserver for NoopObserver {}

struct SupervisedTask {
    shutdown: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl SupervisedTask {
    fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for SupervisedTask {
    fn drop(&mut self) {
        // A lifecycle future may itself be cancelled. Never detach an untracked supervisor:
        // request orderly shutdown, then abort as the synchronous backstop.
        self.request_shutdown();
        self.join.abort();
    }
}

/// Owns one supervised task per opaque target and provides race-free replacement.
pub struct ProcessSupervisorSet {
    config: SupervisorConfig,
    tasks: tokio::sync::Mutex<HashMap<String, SupervisedTask>>,
    /// Serializes compound lifecycle operations so task reconfig cannot race stop/shutdown.
    lifecycle: tokio::sync::Mutex<()>,
    closed: AtomicBool,
}

impl ProcessSupervisorSet {
    pub fn new(config: SupervisorConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            tasks: tokio::sync::Mutex::new(HashMap::new()),
            lifecycle: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        })
    }

    pub async fn start(&self, target_id: &str, spawner: Arc<dyn ChildSpawner>) {
        self.start_observed(target_id, spawner, Arc::new(NoopObserver))
            .await;
    }

    pub async fn start_observed(
        &self,
        target_id: &str,
        spawner: Arc<dyn ChildSpawner>,
        observer: Arc<dyn ProcessObserver>,
    ) {
        let _lifecycle = self.lifecycle.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.stop_locked(target_id).await;
        let (shutdown, receiver) = watch::channel(false);
        let config = self.config.clone();
        // Acquire the map before spawning. There must be no cancellation point between spawn and
        // registration, otherwise dropping `start` could detach an untracked supervisor owner.
        let mut tasks = self.tasks.lock().await;
        let join = tokio::spawn(supervise_observed(spawner, observer, config, receiver));
        tasks.insert(target_id.to_string(), SupervisedTask { shutdown, join });
    }

    pub async fn adopt_observed(
        &self,
        target_id: &str,
        child: Box<dyn SupervisedChild>,
        observer: Arc<dyn ProcessObserver>,
    ) {
        let _lifecycle = self.lifecycle.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.stop_locked(target_id).await;
        let (shutdown, receiver) = watch::channel(false);
        let target = target_id.to_string();
        let mut tasks = self.tasks.lock().await;
        let join = tokio::spawn(supervise_adopted(target.clone(), child, observer, receiver));
        tasks.insert(target, SupervisedTask { shutdown, join });
    }

    async fn stop_locked(&self, target_id: &str) {
        let task = self.tasks.lock().await.remove(target_id);
        if let Some(task) = task {
            let mut tasks = vec![(target_id.to_string(), task)];
            tasks[0].1.request_shutdown();
            join_supervisors_bounded(&mut tasks).await;
        }
    }

    pub async fn stop(&self, target_id: &str) {
        let _lifecycle = self.lifecycle.lock().await;
        self.stop_locked(target_id).await;
    }

    pub async fn shutdown_all(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        self.closed.store(true, Ordering::Release);
        let mut tasks: Vec<_> = self.tasks.lock().await.drain().collect();
        for (_, task) in &tasks {
            task.request_shutdown();
        }
        join_supervisors_bounded(&mut tasks).await;
    }
}

async fn supervise_adopted(
    target_id: String,
    mut child: Box<dyn SupervisedChild>,
    observer: Arc<dyn ProcessObserver>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::select! {
        _ = shutdown.changed() => child.kill().await,
        summary = child.wait_exit() => observer.exited(&target_id, &summary).await,
    }
}

async fn join_supervisors_bounded(tasks: &mut [(String, SupervisedTask)]) {
    let joins = tasks
        .iter_mut()
        .map(|(target_id, task)| async move { (target_id.clone(), (&mut task.join).await) });
    match tokio::time::timeout(SUPERVISOR_JOIN_TIMEOUT, futures::future::join_all(joins)).await {
        Ok(results) => {
            for (target_id, result) in results {
                if let Err(error) = result {
                    warn!(%target_id, %error, "process supervisor task failed while shutting down");
                }
            }
        }
        Err(_) => {
            warn!(
                supervisors = tasks.len(),
                timeout_secs = SUPERVISOR_JOIN_TIMEOUT.as_secs(),
                "process supervisor shutdown timed out; aborting remaining supervisors"
            );
            for (_, task) in tasks.iter_mut() {
                task.join.abort();
            }
            let aborted = futures::future::join_all(
                tasks
                    .iter_mut()
                    .map(|(_, task)| async { (&mut task.join).await }),
            );
            if tokio::time::timeout(ABORTED_JOIN_TIMEOUT, aborted)
                .await
                .is_err()
            {
                error!(
                    timeout_secs = ABORTED_JOIN_TIMEOUT.as_secs(),
                    "aborted process supervisors did not finish promptly; continuing shutdown"
                );
            }
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
                warn!(%error, process_group = self.process_group, "failed to signal external process group");
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
        // If the direct child crashed, do not leave credential-bearing descendants behind.
        self.signal_group(libc::SIGKILL);
        outcome
    }

    async fn kill(&mut self) {
        // Give the whole isolated group a chance to exit, then force-clean every descendant.
        self.signal_group(libc::SIGTERM);
        let force_kill = match tokio::time::timeout(TERMINATE_WAIT_TIMEOUT, self.child.wait()).await
        {
            Ok(Ok(_)) => false,
            Ok(Err(error)) => {
                warn!(%error, process_group = self.process_group, "failed waiting for external child after SIGTERM; forcing cleanup");
                true
            }
            Err(_) => {
                warn!(
                    process_group = self.process_group,
                    timeout_secs = TERMINATE_WAIT_TIMEOUT.as_secs(),
                    "external child did not exit after SIGTERM; sending SIGKILL"
                );
                true
            }
        };
        if force_kill {
            self.signal_group(libc::SIGKILL);
            match tokio::time::timeout(FORCE_KILL_WAIT_TIMEOUT, self.child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    warn!(%error, process_group = self.process_group, "failed to reap external child after SIGKILL; continuing shutdown")
                }
                Err(_) => error!(
                    process_group = self.process_group,
                    timeout_secs = FORCE_KILL_WAIT_TIMEOUT.as_secs(),
                    "external child did not report exit after SIGKILL; continuing shutdown"
                ),
            }
        }
        // The direct child may have exited while credential-bearing descendants remain.
        self.signal_group(libc::SIGKILL);
    }

    fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

/// Production spawner for an external child executable.
///
/// 資格情報は指定された子の env だけへ渡し、argv・config・ログには載せない。
/// `kill_on_drop(true)` で監視タスクが drop されても子を確実に殺す（孤児防止の backstop）。
pub struct ExternalProcessSpawner {
    bin: std::path::PathBuf,
    config_path: std::path::PathBuf,
    /// 秘密。Debug 導出しない・ログに出さない。
    secret: String,
    secret_env: &'static str,
    service_name: &'static str,
    target_id: String,
}

impl ExternalProcessSpawner {
    /// 任意の外部 service 用。復号済み資格情報は子 env だけへ注入し、
    /// config/argv へ載せない。
    pub fn with_secret_env(
        bin: std::path::PathBuf,
        config_path: std::path::PathBuf,
        secret: String,
        secret_env: &'static str,
        service_name: &'static str,
        target_id: String,
    ) -> Self {
        Self {
            bin,
            config_path,
            secret,
            secret_env,
            service_name,
            target_id,
        }
    }
}

#[async_trait::async_trait]
impl ChildSpawner for ExternalProcessSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg(&self.config_path);
        cmd.env(self.secret_env, &self.secret);
        cmd.kill_on_drop(true);
        // External services may spawn helpers. Keep each process tree in an isolated group so
        // stop/restart/shutdown cannot orphan them.
        std::os::unix::process::CommandExt::process_group(cmd.as_std_mut(), 0);
        let child = cmd.spawn()?;
        let process_group = child.id().ok_or_else(|| {
            std::io::Error::other("spawned external child did not expose a process id")
        })? as i32;
        Ok(Box::new(TokioChild {
            child,
            process_group,
        }))
    }

    fn target_id(&self) -> &str {
        &self.target_id
    }

    fn service_name(&self) -> &str {
        self.service_name
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
    shutdown: watch::Receiver<bool>,
) {
    supervise_observed(spawner, Arc::new(NoopObserver), cfg, shutdown).await;
}

pub async fn supervise_observed(
    spawner: Arc<dyn ChildSpawner>,
    observer: Arc<dyn ProcessObserver>,
    cfg: SupervisorConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let target_id = spawner.target_id().to_string();
    let service_name = spawner.service_name().to_string();
    let mut consecutive: u32 = 0;

    loop {
        if *shutdown.borrow() {
            break;
        }

        // ---- spawn ----
        let mut child = match spawner.spawn().await {
            Ok(c) => {
                let pid = c.pid();
                info!(
                    target_id = %target_id,
                    service = %service_name,
                    pid = ?pid,
                    "external child started under supervision; credential injected by env"
                );
                observer.spawned(&target_id, pid).await;
                c
            }
            Err(e) => {
                consecutive = consecutive.saturating_add(1);
                let delay = backoff_delay(consecutive, cfg.base_delay, cfg.max_delay);
                escalate_spawn_failed(&target_id, consecutive, &e.to_string(), delay.as_secs());
                observer.spawn_failed(&target_id).await;
                if !cfg.restart_on_exit {
                    break;
                }
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
                        target_id = %target_id,
                        service = %service_name,
                        outcome = %outcome,
                        "external child exited during shutdown (expected; no restart)"
                    );
                    break;
                }
                let uptime = started.elapsed();
                consecutive = next_consecutive(consecutive, uptime, cfg.reset_after);
                let delay = backoff_delay(consecutive, cfg.base_delay, cfg.max_delay);
                escalate_child_exited(
                    &target_id,
                    consecutive,
                    uptime.as_secs(),
                    &outcome,
                    delay.as_secs(),
                    cfg.crash_loop_threshold,
                );
                observer.exited(&target_id, &outcome).await;
                if !cfg.restart_on_exit {
                    break;
                }
                if sleep_or_shutdown(&mut shutdown, delay).await {
                    break;
                }
                // → loop 先頭へ戻って再 spawn。
            }
            _ = wait_for_shutdown(&mut shutdown) => {
                // 生きている子を terminate（孤児防止）。再起動はしない。
                info!(target_id = %target_id, service = %service_name, "terminating external child on shutdown");
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
#[path = "tests.rs"]
mod tests;
