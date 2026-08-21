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

// 旧 `HeartbeatDecision`（`Speak` / `Learn` / `Idle` / `ManageSkills`）は #588 Stage 3 で撤去した。
// ハートビートは専用の語彙を持たず通常のターンとして走るようになり（応答本文をそのまま扱う）、
// この列挙型は live のロジックから完全に消えた。`Learn` が書いていた内省（reflection）は
// `memory_maintenance` / `learning.rs` の経路が担っており、ハートビートから外しても失われない
// （#531）。型として残すと「ハートビートは Speak/Learn/Idle を決める」という実態と食い違う宣言に
// なるため、`core` からの公開ごと撤去した。
