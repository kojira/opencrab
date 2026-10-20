//! D-1081: the said's `model_profile`.

use serde::Serialize;

/// Which session model slot core uses for the turn this said starts. The gateway declares the
/// interaction kind; core owns the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnModelProfile {
    #[default]
    Default,
    /// Latency-sensitive conversational turn.
    Realtime,
}

impl TurnModelProfile {
    pub(super) fn is_default(&self) -> bool {
        *self == Self::Default
    }
}
