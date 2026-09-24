//! Platform-neutral persisted lifecycle state machine used by daemon owners.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Disabled,
    Pending,
    Provisioning,
    Ready,
    Running,
    Error,
}

impl LifecycleState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LifecycleState {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "pending" => Ok(Self::Pending),
            "provisioning" => Ok(Self::Provisioning),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "error" => Ok(Self::Error),
            _ => Err("unknown lifecycle state"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedLifecycle {
    pub desired_generation: u64,
    pub applied_generation: Option<u64>,
    pub enabled: bool,
    pub state: LifecycleState,
    pub retry_at_unix_ms: Option<i64>,
    pub process_nonce: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessObservation {
    Missing,
    ExactLive,
    StaleLive,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreObservation {
    ExactEnabled,
    ExactDisabled,
    Mismatch,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupDecision {
    KeepDisabled,
    EnqueuePending,
    StartReady,
    AdoptRunning,
    PersistError(&'static str),
}

impl PersistedLifecycle {
    pub fn child_may_start(&self) -> bool {
        self.enabled
            && self.state == LifecycleState::Ready
            && self.applied_generation == Some(self.desired_generation)
    }

    /// Deterministic startup normalization from design §7. The caller must stop/reap any process
    /// unless this returns `AdoptRunning`; failed process/core observations never guess.
    pub fn recover(
        &self,
        now_unix_ms: i64,
        process: ProcessObservation,
        core: CoreObservation,
    ) -> StartupDecision {
        use CoreObservation as Core;
        use LifecycleState as State;
        use ProcessObservation as Process;
        if matches!(process, Process::Unknown) || matches!(core, Core::Unavailable) {
            return StartupDecision::PersistError("startup_recovery_failed");
        }
        match self.state {
            State::Disabled => match (process, core) {
                (Process::Missing, Core::ExactDisabled) if !self.enabled => {
                    StartupDecision::KeepDisabled
                }
                _ => StartupDecision::EnqueuePending,
            },
            State::Pending | State::Provisioning => StartupDecision::EnqueuePending,
            State::Ready => match (process, core) {
                (Process::Missing, Core::ExactEnabled) if self.child_may_start() => {
                    StartupDecision::StartReady
                }
                _ => StartupDecision::EnqueuePending,
            },
            State::Running => match (process, core) {
                (Process::ExactLive, Core::ExactEnabled)
                    if self.enabled
                        && self.applied_generation == Some(self.desired_generation)
                        && self.process_nonce.is_some() =>
                {
                    StartupDecision::AdoptRunning
                }
                _ => StartupDecision::PersistError("child_lost"),
            },
            State::Error => {
                if self.applied_generation != Some(self.desired_generation)
                    || self
                        .retry_at_unix_ms
                        .is_none_or(|deadline| deadline <= now_unix_ms)
                {
                    StartupDecision::EnqueuePending
                } else {
                    StartupDecision::PersistError("retry_pending")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: LifecycleState) -> PersistedLifecycle {
        PersistedLifecycle {
            desired_generation: 3,
            applied_generation: Some(3),
            enabled: true,
            state,
            retry_at_unix_ms: Some(200),
            process_nonce: Some("nonce".into()),
        }
    }

    #[test]
    fn s5_only_enabled_verified_ready_can_start() {
        let mut value = row(LifecycleState::Ready);
        assert!(value.child_may_start());
        for state in [
            LifecycleState::Disabled,
            LifecycleState::Pending,
            LifecycleState::Provisioning,
            LifecycleState::Running,
            LifecycleState::Error,
        ] {
            value.state = state;
            assert!(!value.child_may_start(), "{state}");
        }
        value.state = LifecycleState::Ready;
        value.enabled = false;
        assert!(!value.child_may_start());
        value.enabled = true;
        value.applied_generation = Some(2);
        assert!(!value.child_may_start());
    }

    #[test]
    fn s5_startup_recovery_covers_every_persisted_state() {
        let exact = CoreObservation::ExactEnabled;
        assert_eq!(
            row(LifecycleState::Pending).recover(100, ProcessObservation::Missing, exact),
            StartupDecision::EnqueuePending
        );
        assert_eq!(
            row(LifecycleState::Provisioning).recover(100, ProcessObservation::Missing, exact),
            StartupDecision::EnqueuePending
        );
        assert_eq!(
            row(LifecycleState::Ready).recover(100, ProcessObservation::Missing, exact),
            StartupDecision::StartReady
        );
        assert_eq!(
            row(LifecycleState::Running).recover(100, ProcessObservation::ExactLive, exact),
            StartupDecision::AdoptRunning
        );
        assert_eq!(
            row(LifecycleState::Running).recover(100, ProcessObservation::Missing, exact),
            StartupDecision::PersistError("child_lost")
        );
        assert_eq!(
            row(LifecycleState::Error).recover(250, ProcessObservation::Missing, exact),
            StartupDecision::EnqueuePending
        );
        let mut disabled = row(LifecycleState::Disabled);
        disabled.enabled = false;
        assert_eq!(
            disabled.recover(
                100,
                ProcessObservation::Missing,
                CoreObservation::ExactDisabled
            ),
            StartupDecision::KeepDisabled
        );
    }
}
