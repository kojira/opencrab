use serde::{Deserialize, Serialize};

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
