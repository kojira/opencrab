use axum::{extract::State, http::StatusCode, Json};

use crate::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelResolutionError {
    InvalidArgs,
    NotFound,
    Ambiguous,
}

/// Resolve exactly against a model-choice snapshot. Fuzzy matching belongs to gateway UX only.
pub(crate) fn resolve_exact_model(
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

/// The shared, fail-closed availability source for dashboard and gateway model administration.
pub(crate) async fn available_models(state: &AppState) -> anyhow::Result<Vec<String>> {
    let mut choices = Vec::new();
    let router = state.llm_router.get();
    for pname in router.provider_names() {
        let Some(provider) = router.get_provider(pname) else {
            continue;
        };
        let models = provider
            .clone()
            .available_models()
            .await
            .map_err(|_| anyhow::anyhow!("model catalog unavailable for provider {pname}"))?;
        choices.extend(
            models
                .into_iter()
                .map(|model| format!("{pname}:{}", model.id)),
        );
    }
    choices.sort();
    choices.dedup();
    Ok(choices)
}

/// ダッシュボードのモデルセレクタ用: 既定モデルと各プロバイダの利用可能モデル一覧。
pub async fn model_choices(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let choices = available_models(&state).await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "model_catalog_unavailable"})),
        )
    })?;
    Ok(Json(serde_json::json!({
        "default_model": state.default_model,
        "choices": choices,
    })))
}

#[cfg(test)]
mod slash_model_resolution_contract {
    use std::sync::Arc;

    use async_trait::async_trait;
    use opencrab_actions::{ModelAdminError, ModelAdministration};
    use opencrab_llm::traits::{LlmProvider, ModelInfo};

    use super::{available_models, resolve_exact_model, ModelResolutionError as ResolutionError};

    struct FailingModelCatalog;

    #[async_trait]
    impl LlmProvider for FailingModelCatalog {
        fn name(&self) -> &str {
            "failing"
        }

        async fn available_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
            anyhow::bail!("provider-secret-detail")
        }

        async fn chat_completion(
            &self,
            _request: opencrab_llm::message::ChatRequest,
        ) -> anyhow::Result<opencrab_llm::message::ChatResponse> {
            unreachable!("model catalog test does not issue completions")
        }
    }

    fn available(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[tokio::test]
    async fn provider_failure_makes_the_shared_catalog_fallible() {
        let state = crate::test_app_state();
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(Arc::new(FailingModelCatalog));
        state.llm_router.swap(router);

        assert!(available_models(&state).await.is_err());
        assert_eq!(
            state.list_models("agent-x").await,
            Err(ModelAdminError::Internal)
        );
    }

    #[test]
    fn exact_model_resolution_accepts_full_provider_model() {
        let available = available(&["anthropic:claude-sonnet-4", "openai:gpt-5"]);

        assert_eq!(
            resolve_exact_model(&available, "openai:gpt-5"),
            Ok("openai:gpt-5".to_string())
        );
    }

    #[test]
    fn exact_model_resolution_canonicalizes_unique_bare_id() {
        let available = available(&["anthropic:claude-sonnet-4", "openai:gpt-5"]);

        assert_eq!(
            resolve_exact_model(&available, "gpt-5"),
            Ok("openai:gpt-5".to_string())
        );
    }

    #[test]
    fn exact_model_resolution_rejects_case_whitespace_and_fuzzy_input() {
        let available = available(&["openai:gpt-5"]);
        assert_eq!(
            resolve_exact_model(&available, "GPT-5"),
            Err(ResolutionError::NotFound)
        );
        assert_eq!(
            resolve_exact_model(&available, " gpt-5"),
            Err(ResolutionError::InvalidArgs)
        );
        assert_eq!(
            resolve_exact_model(&available, "gpt"),
            Err(ResolutionError::NotFound)
        );
    }

    #[test]
    fn exact_model_resolution_rejects_ambiguous_bare_id() {
        let available = available(&["azure:gpt-5", "openai:gpt-5"]);

        assert_eq!(
            resolve_exact_model(&available, "gpt-5"),
            Err(ResolutionError::Ambiguous)
        );
    }
}
