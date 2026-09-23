use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use opencrab_actions::{ModelAdminError, ModelAdministration, ModelSnapshot};
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
            ModelAdminError::Internal => Self::internal(),
        }
    }
}

pub struct CommandContext<'a> {
    pub agent_id: &'a str,
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
