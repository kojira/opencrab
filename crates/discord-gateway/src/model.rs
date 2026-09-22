use std::collections::HashSet;

use opencrab_gate_client::client::CommandError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use serenity::all::{CommandOptionType, CreateCommand, CreateCommandOption};

use crate::post::DISCORD_MAX_CHARS;

pub const MODEL_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
pub const AUTOCOMPLETE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1_500);
pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum InteractionAction {
    Command { name: String, args: Value },
    Said,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutocompleteChoice {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ModelInteractionEvent {
    pub event_kind: String,
    pub interaction_id: String,
    pub application_id: String,
    pub token: String,
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub user_id: String,
    pub autocomplete: bool,
    pub subcommand: String,
    pub model: Option<String>,
}

pub(crate) fn model_command() -> CreateCommand {
    let list = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "list",
        "List available models",
    );
    let set = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "set",
        "Set the model for this agent",
    )
    .add_sub_option(
        CreateCommandOption::new(CommandOptionType::String, "model", "Model to use")
            .required(true)
            .set_autocomplete(true),
    );
    let reset = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "reset",
        "Reset to the server default",
    );
    CreateCommand::new("model")
        .description("Inspect or change this agent's model")
        .set_options(vec![list, set, reset])
}

pub(crate) fn map_model_submission(
    subcommand: &str,
    model: Option<&str>,
) -> Option<InteractionAction> {
    let (name, args) = match subcommand {
        "list" if model.is_none() => ("list_models", json!({})),
        "set" => ("set_model", json!({"model": model?})),
        "reset" if model.is_none() => ("reset_model", json!({})),
        _ => return None,
    };
    Some(InteractionAction::Command {
        name: name.to_string(),
        args,
    })
}

pub(crate) fn autocomplete_choices(models: &[String], query: &str) -> Vec<AutocompleteChoice> {
    let query = query.to_lowercase();
    let mut seen = HashSet::new();
    let mut matches = models
        .iter()
        .filter(|canonical| canonical.chars().count() <= 100)
        .filter(|canonical| seen.insert((*canonical).clone()))
        .filter_map(|canonical| {
            let canonical_lower = canonical.to_lowercase();
            let bare_lower = canonical
                .split_once(':')
                .map(|(_, bare)| bare)
                .unwrap_or(canonical)
                .to_lowercase();
            let rank = if canonical_lower.starts_with(&query) {
                0
            } else if bare_lower.starts_with(&query) {
                1
            } else if canonical_lower.contains(&query) || bare_lower.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, canonical.clone()))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    matches
        .into_iter()
        .take(25)
        .map(|(_, canonical)| AutocompleteChoice {
            name: canonical.clone(),
            value: canonical,
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ModelListResult {
    pub models: Vec<String>,
    pub configured_model: Option<String>,
    pub current_model: String,
    pub default_model: String,
}

#[derive(Debug, Deserialize)]
struct ModelMutationResult {
    configured_model: Option<String>,
    current_model: String,
    default_model: String,
    applies: String,
}

pub(crate) fn list_result(value: Value) -> Option<ModelListResult> {
    serde_json::from_value(value).ok()
}

pub(crate) fn submission_success(name: &str, value: Value) -> Option<String> {
    if name == "list_models" {
        return list_result(value).map(|result| format_model_list(&result));
    }
    let result: ModelMutationResult = serde_json::from_value(value).ok()?;
    if result.applies != "next_turn" {
        return None;
    }
    match name {
        "set_model" => Some(format!(
            "Model set to `{}`. It will be used from the next turn.",
            escape_inline(result.configured_model.as_deref()?)
        )),
        "reset_model" => {
            if result.configured_model.is_some() || result.current_model != result.default_model {
                return None;
            }
            Some(format!(
                "Model reset to the server default `{}`. It will be used from the next turn.",
                escape_inline(&result.default_model)
            ))
        }
        _ => None,
    }
}

pub(crate) fn command_error_text(input: Option<&str>, error: &CommandError) -> String {
    match error {
        CommandError::Rejected { code, .. } if code == "forbidden" => {
            "Only an owner can use /model.".to_string()
        }
        CommandError::Rejected { code, .. } if code == "model_not_found" => format!(
            "Unknown model `{}`. Choose a model from autocomplete or use provider:model.",
            escape_inline(input.unwrap_or_default())
        ),
        CommandError::Rejected { code, .. } if code == "model_ambiguous" => format!(
            "Model `{}` exists for multiple providers. Use provider:model.",
            escape_inline(input.unwrap_or_default())
        ),
        CommandError::Rejected { code, message } if code == "model_validation_failed" => {
            format!(
                "Could not set model: {}",
                escape_markdown(message.as_deref().unwrap_or_default())
            )
        }
        CommandError::Rejected { code, .. } if code == "unknown_message" => {
            "This server does not support /model yet (unknown_message).".to_string()
        }
        CommandError::Timeout => "The model command timed out. Try again.".to_string(),
        CommandError::Rejected { code, message } => format!(
            "The model command failed: {}",
            escape_markdown(message.as_deref().unwrap_or(code))
        ),
        CommandError::NotReady => {
            "The model command failed: The binding is not available.".to_string()
        }
        CommandError::Disconnected => "The model command failed: disconnect".to_string(),
    }
}

pub(crate) fn format_model_list(result: &ModelListResult) -> String {
    let _configured_model = result.configured_model.as_deref();
    let current = escape_inline(&result.current_model);
    let default = escape_inline(&result.default_model);
    let mut models = result.models.clone();
    models.sort();
    models.dedup();
    if models.is_empty() {
        return format!(
            "No models are currently available.\n\nCurrent: `{current}`\nDefault: `{default}`"
        );
    }

    let escaped = models
        .iter()
        .map(|model| format!("`{}`", escape_inline(model)))
        .collect::<Vec<_>>();
    for shown in (0..=escaped.len()).rev() {
        let mut section = escaped[..shown].join("\n");
        let remaining = escaped.len() - shown;
        if remaining > 0 {
            if !section.is_empty() {
                section.push('\n');
            }
            section.push_str(&format!("… and {remaining} more."));
        }
        let response =
            format!("Available models:\n{section}\n\nCurrent: `{current}`\nDefault: `{default}`");
        if response.chars().count() <= DISCORD_MAX_CHARS {
            return response;
        }
    }
    // Current/default values originate from canonical server configuration and are normally short.
    // Keep the response valid even for corrupt overlong state.
    "The model command failed: response exceeds Discord's message limit".to_string()
}

fn escape_inline(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            character if character.is_control() => escaped.push('�'),
            '\\' | '`' => {
                escaped.push('\\');
                escaped.push(character);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '~' | '|' | '>' | '#' | '-' | '@'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_definition_has_only_the_three_model_subcommands() {
        let value = serde_json::to_value(model_command()).unwrap();
        assert_eq!(value["name"], "model");
        let options = value["options"].as_array().unwrap();
        assert_eq!(
            options
                .iter()
                .map(|option| option["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["list", "set", "reset"]
        );
        assert_eq!(options[1]["options"][0]["name"], "model");
        assert_eq!(options[1]["options"][0]["required"], true);
        assert_eq!(options[1]["options"][0]["autocomplete"], true);
    }

    #[test]
    fn success_and_stable_errors_use_exact_discord_text() {
        assert_eq!(
            submission_success(
                "set_model",
                json!({
                    "configured_model": "openai:gpt-5",
                    "current_model": "openai:gpt-5",
                    "default_model": "anthropic:claude-sonnet-4",
                    "applies": "next_turn"
                })
            )
            .as_deref(),
            Some("Model set to `openai:gpt-5`. It will be used from the next turn.")
        );
        assert_eq!(
            command_error_text(
                Some("gpt-5"),
                &CommandError::Rejected {
                    code: "model_ambiguous".to_string(),
                    message: None,
                }
            ),
            "Model `gpt-5` exists for multiple providers. Use provider:model."
        );
        assert_eq!(
            command_error_text(
                None,
                &CommandError::Rejected {
                    code: "unknown_message".to_string(),
                    message: None,
                }
            ),
            "This server does not support /model yet (unknown_message)."
        );
    }

    #[test]
    fn dynamic_model_ids_cannot_break_out_of_inline_code() {
        let result = ModelListResult {
            models: vec!["provider:safe`\n# [forged](https://example.invalid)".to_string()],
            configured_model: None,
            current_model: "provider:current\r\n> forged".to_string(),
            default_model: "provider:default".to_string(),
        };

        let text = format_model_list(&result);

        assert!(text.contains("`provider:safe\\`\\n# [forged](https://example.invalid)`"));
        assert!(text.contains("Current: `provider:current\\r\\n> forged`"));
        assert!(!text.contains("\n# [forged]"));
        assert!(!text.contains("\n> forged"));
    }

    #[test]
    fn list_truncation_keeps_complete_lines_and_suffix() {
        let result = ModelListResult {
            models: (0..200)
                .map(|index| format!("provider:model-{index:03}"))
                .collect(),
            configured_model: None,
            current_model: "provider:current".to_string(),
            default_model: "provider:default".to_string(),
        };
        let text = format_model_list(&result);
        assert!(text.chars().count() <= DISCORD_MAX_CHARS);
        assert!(text.contains("… and "));
        assert!(text.ends_with("Default: `provider:default`"));
    }
}
