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
}
