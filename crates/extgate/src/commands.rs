use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use opencrab_actions::{
    EffortSnapshot, ModelAdminError, ModelAdministration, ModelSnapshot, SessionEffortList,
    SessionModelList, TurnModelProfile, REASONING_EFFORTS,
};
use opencrab_db::queries::ReasoningEffortSource;
use serde_json::{json, Map, Value};

use crate::protocol::SaidCaller;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub code: &'static str,
    pub message: String,
}

impl CommandError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn forbidden() -> Self {
        Self::new("forbidden", "Only an owner may use model commands.")
    }

    pub fn binding_not_found() -> Self {
        Self::new("binding_not_found", "The binding is not available.")
    }

    pub fn internal() -> Self {
        Self::new("internal", "The model command could not be completed.")
    }

    fn invalid_args() -> Self {
        Self::new("invalid_args", "Invalid command arguments.")
    }

    fn from_model(error: ModelAdminError) -> Self {
        match error {
            ModelAdminError::NotFound => {
                Self::new("model_not_found", "The requested model is not available.")
            }
            ModelAdminError::Ambiguous => Self::new(
                "model_ambiguous",
                "The model ID is available from multiple providers.",
            ),
            ModelAdminError::InvalidArgs => Self::invalid_args(),
            ModelAdminError::Validation(message) => Self::new("model_validation_failed", message),
            ModelAdminError::TargetUnavailable => Self::binding_not_found(),
            ModelAdminError::Internal => Self::internal(),
        }
    }
}

pub struct CommandContext<'a> {
    pub agent_id: &'a str,
    /// Canonical session of the authenticated binding (never taken from command args).
    pub session_id: &'a str,
    pub caller: &'a SaidCaller,
    pub models: &'a dyn ModelAdministration,
}

#[async_trait]
pub trait CommandHandler: Send + Sync {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError>;
}

#[derive(Default)]
pub struct CommandRegistry {
    handlers: HashMap<String, Arc<dyn CommandHandler>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_builtins() -> Result<Self, String> {
        let mut registry = Self::new();
        registry.register("list_models", Arc::new(ListModels))?;
        registry.register("set_model", Arc::new(SetModel))?;
        registry.register("reset_model", Arc::new(ResetModel))?;
        registry.register("list_session_models", Arc::new(ListSessionModels))?;
        registry.register("set_session_model", Arc::new(SetSessionModel))?;
        registry.register("reset_session_model", Arc::new(ResetSessionModel))?;
        registry.register(
            "list_session_reasoning_efforts",
            Arc::new(ListSessionReasoningEfforts),
        )?;
        registry.register(
            "set_session_reasoning_effort",
            Arc::new(SetSessionReasoningEffort),
        )?;
        registry.register(
            "reset_session_reasoning_effort",
            Arc::new(ResetSessionReasoningEffort),
        )?;
        Ok(registry)
    }

    pub fn register(
        &mut self,
        name: impl Into<String>,
        handler: Arc<dyn CommandHandler>,
    ) -> Result<(), String> {
        let name = name.into();
        if self.handlers.contains_key(&name) {
            return Err(format!("duplicate command: {name}"));
        }
        self.handlers.insert(name, handler);
        Ok(())
    }

    pub async fn dispatch(
        &self,
        name: &str,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        let Some(handler) = self.handlers.get(name) else {
            return Err(CommandError::new(
                "unknown_command",
                "The command is not available.",
            ));
        };
        handler.handle(context, args).await
    }
}

fn list_result(snapshot: ModelSnapshot) -> Value {
    json!({
        "models": snapshot.models,
        "configured_model": snapshot.configured_model,
        "current_model": snapshot.current_model,
        "default_model": snapshot.default_model,
    })
}

fn mutation_result(snapshot: ModelSnapshot) -> Value {
    json!({
        "configured_model": snapshot.configured_model,
        "current_model": snapshot.current_model,
        "default_model": snapshot.default_model,
        "applies": "next_turn",
    })
}

struct ListModels;

#[async_trait]
impl CommandHandler for ListModels {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        if !args.is_empty() {
            return Err(CommandError::invalid_args());
        }
        context
            .models
            .list_models(context.agent_id)
            .await
            .map(list_result)
            .map_err(CommandError::from_model)
    }
}

struct SetModel;

#[async_trait]
impl CommandHandler for SetModel {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        if args.len() != 1 {
            return Err(CommandError::invalid_args());
        }
        let Some(model) = args.get("model").and_then(Value::as_str) else {
            return Err(CommandError::invalid_args());
        };
        context
            .models
            .set_model(context.agent_id, model)
            .await
            .map(mutation_result)
            .map_err(CommandError::from_model)
    }
}

struct ResetModel;

#[async_trait]
impl CommandHandler for ResetModel {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        if !args.is_empty() {
            return Err(CommandError::invalid_args());
        }
        context
            .models
            .reset_model(context.agent_id)
            .await
            .map(mutation_result)
            .map_err(CommandError::from_model)
    }
}

fn session_list_result(list: SessionModelList) -> Value {
    let snapshot = list.snapshot;
    json!({
        "scope": "session",
        "models": snapshot.models,
        "configured_model": snapshot.configured_model,
        "current_model": snapshot.current_model,
        "default_model": snapshot.default_model,
        "realtime_configured_model": list.realtime_configured_model,
        "realtime_current_model": list.realtime_current_model,
    })
}

fn session_mutation_result(profile: TurnModelProfile, snapshot: ModelSnapshot) -> Value {
    let mut result = json!({
        "scope": "session",
        "configured_model": snapshot.configured_model,
        "current_model": snapshot.current_model,
        "default_model": snapshot.default_model,
        "applies": "next_turn",
    });
    if profile == TurnModelProfile::Realtime {
        result["profile"] = json!("realtime");
    }
    result
}

/// Session mutations accept their own keys plus an optional `profile:"realtime"`; any other key
/// or profile value is `invalid_args`.
fn session_profile_arg(
    args: &Map<String, Value>,
    own_keys: &[&str],
) -> Result<TurnModelProfile, CommandError> {
    let profile = match args.get("profile") {
        None => TurnModelProfile::Default,
        Some(Value::String(profile)) if profile == "realtime" => TurnModelProfile::Realtime,
        Some(_) => return Err(CommandError::invalid_args()),
    };
    let expected = own_keys.len() + usize::from(profile == TurnModelProfile::Realtime);
    if args.len() != expected || !own_keys.iter().all(|key| args.contains_key(*key)) {
        return Err(CommandError::invalid_args());
    }
    Ok(profile)
}

struct ListSessionModels;

#[async_trait]
impl CommandHandler for ListSessionModels {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        if !args.is_empty() {
            return Err(CommandError::invalid_args());
        }
        context
            .models
            .list_session_models(context.agent_id, context.session_id)
            .await
            .map(session_list_result)
            .map_err(CommandError::from_model)
    }
}

struct SetSessionModel;

#[async_trait]
impl CommandHandler for SetSessionModel {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        let profile = session_profile_arg(args, &["model"])?;
        let Some(model) = args.get("model").and_then(Value::as_str) else {
            return Err(CommandError::invalid_args());
        };
        context
            .models
            .set_session_model(context.agent_id, context.session_id, profile, model)
            .await
            .map(|snapshot| session_mutation_result(profile, snapshot))
            .map_err(CommandError::from_model)
    }
}

struct ResetSessionModel;

#[async_trait]
impl CommandHandler for ResetSessionModel {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        let profile = session_profile_arg(args, &[])?;
        context
            .models
            .reset_session_model(context.agent_id, context.session_id, profile)
            .await
            .map(|snapshot| session_mutation_result(profile, snapshot))
            .map_err(CommandError::from_model)
    }
}

fn effort_source(source: ReasoningEffortSource) -> &'static str {
    match source {
        ReasoningEffortSource::Session => "session",
        ReasoningEffortSource::Agent => "agent",
        ReasoningEffortSource::Provider => "provider",
    }
}

fn effort_list_result(list: SessionEffortList) -> Value {
    json!({
        "scope": "session",
        "efforts": REASONING_EFFORTS,
        "configured_effort": list.default.configured_effort,
        "current_effort": list.default.current_effort,
        "source": effort_source(list.default.source),
        "realtime_configured_effort": list.realtime.configured_effort,
        "realtime_current_effort": list.realtime.current_effort,
        "realtime_source": effort_source(list.realtime.source),
    })
}

fn effort_mutation_result(profile: TurnModelProfile, snapshot: EffortSnapshot) -> Value {
    let mut result = json!({
        "scope": "session",
        "configured_effort": snapshot.configured_effort,
        "current_effort": snapshot.current_effort,
        "source": effort_source(snapshot.source),
        "applies": "next_turn",
    });
    if profile == TurnModelProfile::Realtime {
        result["profile"] = json!("realtime");
    }
    result
}

struct ListSessionReasoningEfforts;

#[async_trait]
impl CommandHandler for ListSessionReasoningEfforts {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        if !args.is_empty() {
            return Err(CommandError::invalid_args());
        }
        context
            .models
            .list_session_reasoning_efforts(context.agent_id, context.session_id)
            .await
            .map(effort_list_result)
            .map_err(CommandError::from_model)
    }
}

struct SetSessionReasoningEffort;

#[async_trait]
impl CommandHandler for SetSessionReasoningEffort {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        let profile = session_profile_arg(args, &["effort"])?;
        let Some(effort) = args.get("effort").and_then(Value::as_str) else {
            return Err(CommandError::invalid_args());
        };
        context
            .models
            .set_session_reasoning_effort(context.agent_id, context.session_id, profile, effort)
            .await
            .map(|snapshot| effort_mutation_result(profile, snapshot))
            .map_err(CommandError::from_model)
    }
}

struct ResetSessionReasoningEffort;

#[async_trait]
impl CommandHandler for ResetSessionReasoningEffort {
    async fn handle(
        &self,
        context: CommandContext<'_>,
        args: &Map<String, Value>,
    ) -> Result<Value, CommandError> {
        let profile = session_profile_arg(args, &[])?;
        context
            .models
            .reset_session_reasoning_effort(context.agent_id, context.session_id, profile)
            .await
            .map(|snapshot| effort_mutation_result(profile, snapshot))
            .map_err(CommandError::from_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    #[async_trait]
    impl CommandHandler for Noop {
        async fn handle(
            &self,
            _context: CommandContext<'_>,
            _args: &Map<String, Value>,
        ) -> Result<Value, CommandError> {
            Ok(Value::Null)
        }
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut registry = CommandRegistry::new();
        registry.register("same", Arc::new(Noop)).unwrap();
        assert_eq!(
            registry.register("same", Arc::new(Noop)),
            Err("duplicate command: same".to_string())
        );
    }
}
