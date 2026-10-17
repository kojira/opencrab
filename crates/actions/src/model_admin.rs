use async_trait::async_trait;

/// Provider-neutral snapshot used by gateway commands that administer an agent's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSnapshot {
    pub models: Vec<String>,
    pub configured_model: Option<String>,
    pub current_model: String,
    pub default_model: String,
}

/// D-1081: which stored model slot a session turn uses. `Realtime` marks a latency-sensitive
/// conversational turn declared by the gateway; the gateway never names a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnModelProfile {
    #[default]
    Default,
    Realtime,
}

/// Session-scoped list: the default-profile snapshot plus the realtime slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModelList {
    pub snapshot: ModelSnapshot,
    /// The session's realtime override. `None` = realtime turns follow `snapshot.current_model`.
    pub realtime_configured_model: Option<String>,
    /// The effective model of the session's next realtime turn.
    pub realtime_current_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAdminError {
    NotFound,
    Ambiguous,
    InvalidArgs,
    Validation(String),
    /// The command target (session and its single member agent) is not available.
    TargetUnavailable,
    Internal,
}

/// Server-owned model administration boundary. Callers identify an already-resolved agent;
/// implementations own availability, validation, and persistence.
#[async_trait]
pub trait ModelAdministration: Send + Sync {
    async fn list_models(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError>;

    async fn set_model(
        &self,
        agent_id: &str,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError>;

    async fn reset_model(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError>;

    /// Session-scoped list: `configured_model` is the session override, `current_model` the
    /// effective model for the session's next turn, `default_model` the agent's effective model,
    /// plus the realtime slot. `session_id` must have exactly `[agent_id]` as members.
    async fn list_session_models(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<SessionModelList, ModelAdminError>;

    /// Persists the `profile` slot of the session for its next turn. Never waits for a turn.
    /// For `Realtime`, `configured_model` is the realtime slot, `current_model` the effective
    /// realtime model and `default_model` the session's default-profile effective model.
    async fn set_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError>;

    /// Clears the `profile` slot so it follows its inherited model again (`Default`: the agent's
    /// model; `Realtime`: the session's default-profile effective model).
    async fn reset_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<ModelSnapshot, ModelAdminError>;
}
