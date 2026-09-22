use serde_json::json;

use crate::model::{
    autocomplete_choices, map_model_submission, AutocompleteChoice, InteractionAction,
};

#[test]
fn model_interaction_maps_to_command_and_never_to_said() {
    let mapped = map_model_submission("set", Some("openai:gpt-5"));
    assert_ne!(mapped, Some(InteractionAction::Said));
    assert_eq!(
        mapped,
        Some(InteractionAction::Command {
            name: "set_model".to_string(),
            args: json!({"model": "openai:gpt-5"}),
        })
    );
}

#[test]
fn model_autocomplete_orders_case_insensitive_matches_and_returns_canonical_values() {
    let models = [
        "zeta:model-gpt",
        "openai:gpt-z",
        "gptcloud:model-z",
        "anthropic:GPT-a",
        "none:model",
    ]
    .map(str::to_string);

    assert_eq!(
        autocomplete_choices(&models, "GpT"),
        [
            "gptcloud:model-z",
            "anthropic:GPT-a",
            "openai:gpt-z",
            "zeta:model-gpt",
        ]
        .map(|canonical| AutocompleteChoice {
            name: canonical.to_string(),
            value: canonical.to_string(),
        })
    );
}

#[test]
fn model_autocomplete_caps_at_25_and_omits_overlong_canonical_values() {
    let mut models = (0..30)
        .map(|index| format!("provider-{index:02}:gpt-{index:02}"))
        .collect::<Vec<_>>();
    let overlong = format!("provider:gpt-{}", "x".repeat(100));
    assert!(overlong.chars().count() > 100);
    models.push(overlong.clone());

    let choices = autocomplete_choices(&models, "gpt");

    assert_eq!(choices.len(), 25);
    assert_eq!(choices[0].value, "provider-00:gpt-00");
    assert_eq!(choices[24].value, "provider-24:gpt-24");
    assert!(choices.iter().all(|choice| choice.name == choice.value));
    assert!(choices.iter().all(|choice| choice.value != overlong));
}
