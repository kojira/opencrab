use async_trait::async_trait;

/// A registered provider whose model list could not be read, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableProvider {
    pub provider: String,
    pub error: String,
}

/// The selectable models (`provider:model`, sorted, deduplicated) of the providers that listed
/// their models, plus the providers that could not. One provider failing never hides the others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    pub models: Vec<String>,
    pub unavailable_providers: Vec<UnavailableProvider>,
}

impl ModelCatalog {
    /// `[{"provider": …, "error": …}]`: the JSON form the REST API, the gate commands and the
    /// agent tools all show.
    pub fn unavailable_providers_json(&self) -> serde_json::Value {
        unavailable_providers_json(&self.unavailable_providers)
    }
}

/// See [`ModelCatalog::unavailable_providers_json`].
pub fn unavailable_providers_json(providers: &[UnavailableProvider]) -> serde_json::Value {
    serde_json::Value::Array(
        providers
            .iter()
            .map(|p| serde_json::json!({"provider": p.provider, "error": p.error}))
            .collect(),
    )
}

/// Provider-neutral snapshot used by gateway commands that administer an agent's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSnapshot {
    pub models: Vec<String>,
    /// Providers missing from `models` because their model list could not be read.
    pub unavailable_providers: Vec<UnavailableProvider>,
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

/// D-1086: the reasoning effort values a session slot accepts.
pub const REASONING_EFFORTS: &[&str] = &["none", "low", "medium", "high"];

/// D-1086: one reasoning effort slot of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortSnapshot {
    /// The session's override in this slot. `None` = not set.
    pub configured_effort: Option<String>,
    /// The effort the slot's next turn uses. `None` = the model's own default.
    pub current_effort: Option<String>,
    /// Where `current_effort` comes from.
    pub source: opencrab_db::queries::ReasoningEffortSource,
}

/// D-1086: both reasoning effort slots of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEffortList {
    pub default: EffortSnapshot,
    pub realtime: EffortSnapshot,
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

/// Why [`resolve_exact_model`] could not resolve a model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelResolutionError {
    InvalidArgs,
    NotFound,
    Ambiguous,
}

/// Resolve exactly against a model-choice snapshot ([`ModelSnapshot::models`]). The single
/// resolver for the gateway `/model` commands and the agent's `select_llm` (D-1131). Fuzzy
/// matching belongs to gateway UX only.
pub fn resolve_exact_model(
    available: &[String],
    input: &str,
) -> Result<String, ModelResolutionError> {
    if input.is_empty() || input.trim() != input {
        return Err(ModelResolutionError::InvalidArgs);
    }
    if available.iter().any(|model| model == input) {
        return Ok(input.to_string());
    }
    if input.contains(':') {
        return Err(ModelResolutionError::NotFound);
    }

    let mut matches = available
        .iter()
        .filter(|canonical| canonical.split_once(':').is_some_and(|(_, id)| id == input));
    let Some(first) = matches.next() else {
        return Err(ModelResolutionError::NotFound);
    };
    if matches.next().is_some() {
        return Err(ModelResolutionError::Ambiguous);
    }
    Ok(first.clone())
}

impl From<ModelResolutionError> for ModelAdminError {
    fn from(error: ModelResolutionError) -> Self {
        match error {
            ModelResolutionError::InvalidArgs => ModelAdminError::InvalidArgs,
            ModelResolutionError::NotFound => ModelAdminError::NotFound,
            ModelResolutionError::Ambiguous => ModelAdminError::Ambiguous,
        }
    }
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

    /// D-1086: both reasoning effort slots of the session. `Default` turns use session >
    /// agent > provider configuration; `Realtime` turns use session > provider configuration.
    /// `session_id` must have exactly `[agent_id]` as members.
    async fn list_session_reasoning_efforts(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<SessionEffortList, ModelAdminError>;

    /// D-1086: persists `effort` (one of [`REASONING_EFFORTS`], else `InvalidArgs`) in the
    /// `profile` slot of the session for its next turn. Never waits for a turn.
    async fn set_session_reasoning_effort(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
        effort: &str,
    ) -> Result<EffortSnapshot, ModelAdminError>;

    /// D-1086: clears the `profile` slot so it follows its inherited effort again.
    async fn reset_session_reasoning_effort(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<EffortSnapshot, ModelAdminError>;
}
