use async_trait::async_trait;

/// Provider-neutral snapshot used by gateway commands that administer an agent's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSnapshot {
    pub models: Vec<String>,
    pub configured_model: Option<String>,
    pub current_model: String,
    pub default_model: String,
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

    /// Session-scoped snapshot: `configured_model` is the session override, `current_model` the
    /// effective model for the session's next turn, `default_model` the agent's effective model.
    /// `session_id` must have exactly `[agent_id]` as members.
    async fn list_session_models(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<ModelSnapshot, ModelAdminError>;

    /// Persists a session model override for the session's next turn. Never waits for a turn.
    async fn set_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError>;

    /// Clears the session override so the session follows the agent's model again.
    async fn reset_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<ModelSnapshot, ModelAdminError>;
}
