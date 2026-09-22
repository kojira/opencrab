use axum::{extract::State, Json};

use crate::AppState;

/// ダッシュボードのモデルセレクタ用: 既定モデルと各プロバイダの利用可能モデル一覧。
pub async fn model_choices(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut choices: Vec<String> = Vec::new();
    let router = state.llm_router.get();
    for pname in router.provider_names() {
        let Some(prov) = router.get_provider(pname) else {
            continue;
        };
        let prov = prov.clone();
        if let Ok(models) = prov.available_models().await {
            for m in models {
                choices.push(format!("{pname}:{}", m.id));
            }
        }
    }
    choices.sort();
    choices.dedup();
    Json(serde_json::json!({
        "default_model": state.default_model,
        "choices": choices,
    }))
}

#[cfg(test)]
mod slash_model_resolution_contract {
    #[derive(Debug, PartialEq, Eq)]
    enum ResolutionError {
        NotFound,
        Ambiguous,
    }

    // RED scaffold: the server command implementation will replace this stand-in with its exact
    // resolver over the model-choice snapshot. Keeping it test-local avoids production behavior.
    fn resolve_exact_model(_available: &[&str], _input: &str) -> Result<String, ResolutionError> {
        Err(ResolutionError::NotFound)
    }

    #[test]
    fn exact_model_resolution_accepts_full_provider_model() {
        let available = ["anthropic:claude-sonnet-4", "openai:gpt-5"];

        assert_eq!(
            resolve_exact_model(&available, "openai:gpt-5"),
            Ok("openai:gpt-5".to_string())
        );
    }

    #[test]
    fn exact_model_resolution_canonicalizes_unique_bare_id() {
        let available = ["anthropic:claude-sonnet-4", "openai:gpt-5"];

        assert_eq!(
            resolve_exact_model(&available, "gpt-5"),
            Ok("openai:gpt-5".to_string())
        );
    }

    #[test]
    fn exact_model_resolution_rejects_ambiguous_bare_id() {
        let available = ["azure:gpt-5", "openai:gpt-5"];

        assert_eq!(
            resolve_exact_model(&available, "gpt-5"),
            Err(ResolutionError::Ambiguous)
        );
    }
}
