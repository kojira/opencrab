use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use tokio::sync::watch;
use tracing;

/// Configuration for the heartbeat loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatConfig {
    /// Interval in seconds between heartbeat ticks.
    /// Defaults to 7 (a prime number, to avoid synchronization patterns).
    pub interval_secs: u64,
    /// Whether the heartbeat is enabled.
    pub enabled: bool,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_secs: 7,
            enabled: false,
        }
    }
}

/// The decision made during a heartbeat tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeartbeatDecision {
    /// The agent decided to say something.
    Speak(String),
    /// The agent decided to learn or reflect.
    Learn,
    /// The agent decided to do nothing.
    Idle,
    /// The agent decided to manage skills (cleanup duplicates, archive unused).
    ManageSkills {
        duplicates_found: usize,
        archived_count: usize,
    },
}

impl std::fmt::Display for HeartbeatDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeartbeatDecision::Speak(msg) => write!(f, "speak: {}", msg),
            HeartbeatDecision::Learn => write!(f, "learn"),
            HeartbeatDecision::Idle => write!(f, "idle"),
            HeartbeatDecision::ManageSkills {
                duplicates_found,
                archived_count,
            } => {
                write!(
                    f,
                    "manage_skills: duplicates={}, archived={}",
                    duplicates_found, archived_count
                )
            }
        }
    }
}

/// Callback type for heartbeat tick processing.
///
/// The callback receives the agent_id and tick count, and returns a decision.
pub type HeartbeatCallback = Box<
    dyn Fn(&str, u64) -> Pin<Box<dyn Future<Output = HeartbeatDecision> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// 各周期の sleep 長（秒）を毎回解決する関数（#439 部分先行）。
///
/// `heartbeat_loop` に渡すと、`config.interval_secs` で固定せず**毎周期の頭で**これを
/// 呼んで次の sleep を決める。呼び出し側が設定を読み直すことで、ループの目の細かさを
/// 設定へ追従させる（発火するかどうかの判定は従来どおりコールバック側のゲート）。
/// 同期関数なので `.await` を跨がずに済ませること（DB ロック等を握ったままにしない）。
pub type HeartbeatIntervalResolver = std::sync::Arc<dyn Fn() -> u64 + Send + Sync + 'static>;

/// Run the heartbeat loop for an agent.
///
/// The loop fires at the configured interval (prime-numbered seconds by default)
/// and invokes the callback on each tick. It runs until a shutdown signal is received.
///
/// # Arguments
/// * `agent_id` - The ID of the agent owning this heartbeat.
/// * `config` - Heartbeat configuration.
/// * `callback` - Function called on each tick to decide what to do.
/// * `interval_resolver` - `Some` なら**毎周期の頭で**呼んで次の sleep 長（秒）を決める
///   （`config.interval_secs` は初期ログ用の値になる）。`None` なら従来どおり固定周期。
/// * `shutdown_rx` - A watch receiver; the loop exits when this becomes `true`.
pub async fn heartbeat_loop(
    agent_id: String,
    config: HeartbeatConfig,
    callback: HeartbeatCallback,
    interval_resolver: Option<HeartbeatIntervalResolver>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    if !config.enabled {
        tracing::info!(agent_id = %agent_id, "Heartbeat disabled, not starting loop");
        return;
    }

    let mut interval_secs = config.interval_secs;
    let mut tick_count: u64 = 0;

    tracing::info!(
        agent_id = %agent_id,
        interval_secs = config.interval_secs,
        dynamic = interval_resolver.is_some(),
        "Starting heartbeat loop"
    );

    loop {
        // 周期の頭で解決し直す。設定変更が次周期から効く（#439 部分先行）。
        if let Some(resolve) = interval_resolver.as_ref() {
            let next = resolve().max(1);
            if next != interval_secs {
                tracing::info!(
                    agent_id = %agent_id,
                    prev_interval_secs = interval_secs,
                    interval_secs = next,
                    "Heartbeat loop interval changed"
                );
                interval_secs = next;
            }
        }
        let interval = tokio::time::Duration::from_secs(interval_secs);

        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                tick_count += 1;

                let decision = callback(&agent_id, tick_count).await;

                tracing::debug!(
                    agent_id = %agent_id,
                    tick = tick_count,
                    decision = %decision,
                    "Heartbeat tick"
                );

                match &decision {
                    HeartbeatDecision::Speak(msg) => {
                        tracing::info!(
                            agent_id = %agent_id,
                            tick = tick_count,
                            message = %msg,
                            "Heartbeat: agent wants to speak"
                        );
                    }
                    HeartbeatDecision::Learn => {
                        tracing::info!(
                            agent_id = %agent_id,
                            tick = tick_count,
                            "Heartbeat: agent wants to learn"
                        );
                    }
                    HeartbeatDecision::Idle => {
                        // Nothing to do.
                    }
                    HeartbeatDecision::ManageSkills { duplicates_found, archived_count } => {
                        tracing::info!(
                            agent_id = %agent_id,
                            tick = tick_count,
                            duplicates_found = duplicates_found,
                            archived_count = archived_count,
                            "Heartbeat: agent managed skills"
                        );
                    }
                }
            }
            changed = shutdown_rx.changed() => {
                // Sender が drop されると changed() は Err を返し続ける。これを無視すると
                // select! が即座に解決し続けて sleep 分岐が飢餓状態になり、CPUを100%回して
                // tick が止まる。チャンネルクローズはシャットダウンとして扱う。
                if changed.is_err() || *shutdown_rx.borrow() {
                    tracing::info!(
                        agent_id = %agent_id,
                        ticks_completed = tick_count,
                        "Heartbeat loop shutting down"
                    );
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// tick 回数を数えるだけのコールバック。
    fn counting_callback(ticks: Arc<AtomicU64>) -> HeartbeatCallback {
        Box::new(move |_agent_id: &str, _tick: u64| {
            let ticks = ticks.clone();
            Box::pin(async move {
                ticks.fetch_add(1, Ordering::SeqCst);
                HeartbeatDecision::Idle
            })
        })
    }

    /// #439 部分先行: sleep 長は**毎周期の頭で**解決し直す。ループ生成時に固定しない。
    ///
    /// 仮想時間で、1 周期目は 600 秒、その後 300 秒へ変えた設定が 2 周期目に効くことを見る。
    /// resolver を初回だけ呼ぶ実装（＝ループ生成時に固定）だと 2 回目の tick は t=1200 に
    /// なるので、t=901 の時点で 1 回しか進んでおらず落ちる。
    #[tokio::test(start_paused = true)]
    async fn loop_resolves_interval_every_cycle() {
        let ticks = Arc::new(AtomicU64::new(0));
        let next_interval = Arc::new(AtomicU64::new(600));

        let interval_for_resolver = next_interval.clone();
        let resolver: HeartbeatIntervalResolver =
            Arc::new(move || interval_for_resolver.load(Ordering::SeqCst));

        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(heartbeat_loop(
            "agent-a".to_string(),
            HeartbeatConfig {
                // config の値は初期ログ用。resolver がある間は sleep を決めない。
                interval_secs: 1800,
                enabled: true,
            },
            counting_callback(ticks.clone()),
            Some(resolver),
            rx,
        ));

        // t=300: 1 周期目（600 秒）の途中。まだ tick していない。
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 0);
        // 周期の途中で設定を変える（現在の sleep には影響しない）。
        next_interval.store(300, Ordering::SeqCst);

        // t=601: 1 周期目は元の 600 秒で発火する。
        tokio::time::sleep(Duration::from_secs(301)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 1);

        // t=901: 2 周期目は再解決した 300 秒で発火する（1800 でも 600 でもない）。
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            2,
            "設定変更が次周期の sleep に反映される"
        );

        tx.send(true).unwrap();
        handle.await.unwrap();
    }

    /// resolver を渡さなければ従来どおり `config.interval_secs` の固定周期（互換）。
    #[tokio::test(start_paused = true)]
    async fn loop_without_resolver_uses_config_interval() {
        let ticks = Arc::new(AtomicU64::new(0));
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(heartbeat_loop(
            "agent-a".to_string(),
            HeartbeatConfig {
                interval_secs: 600,
                enabled: true,
            },
            counting_callback(ticks.clone()),
            None,
            rx,
        ));

        tokio::time::sleep(Duration::from_secs(599)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 0);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 1);

        tx.send(true).unwrap();
        handle.await.unwrap();
    }
}
