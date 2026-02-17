use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing;

/// Configuration for the heartbeat loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl std::fmt::Display for HeartbeatDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeartbeatDecision::Speak(msg) => write!(f, "speak: {}", msg),
            HeartbeatDecision::Learn => write!(f, "learn"),
            HeartbeatDecision::Idle => write!(f, "idle"),
        }
    }
}

/// Callback type for heartbeat tick processing.
///
/// The callback receives the agent_id and tick count, and returns a decision.
pub type HeartbeatCallback =
    Box<dyn Fn(&str, u64) -> HeartbeatDecision + Send + Sync + 'static>;

/// Run the heartbeat loop for an agent.
///
/// The loop fires at the configured interval (prime-numbered seconds by default)
/// and invokes the callback on each tick. It runs until a shutdown signal is received.
///
/// # Arguments
/// * `agent_id` - The ID of the agent owning this heartbeat.
/// * `config` - Heartbeat configuration.
/// * `callback` - Function called on each tick to decide what to do.
/// * `shutdown_rx` - A watch receiver; the loop exits when this becomes `true`.
pub async fn heartbeat_loop(
    agent_id: String,
    config: HeartbeatConfig,
    callback: HeartbeatCallback,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    if !config.enabled {
        tracing::info!(agent_id = %agent_id, "Heartbeat disabled, not starting loop");
        return;
    }

    let interval = tokio::time::Duration::from_secs(config.interval_secs);
    let mut tick_count: u64 = 0;

    tracing::info!(
        agent_id = %agent_id,
        interval_secs = config.interval_secs,
        "Starting heartbeat loop"
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                tick_count += 1;

                let decision = callback(&agent_id, tick_count);

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
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
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
