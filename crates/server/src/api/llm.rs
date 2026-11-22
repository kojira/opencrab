use axum::{extract::State, Json};
use opencrab_actions::{ModelCatalog, UnavailableProvider};

use crate::AppState;

/// The one model catalog for the dashboard, the gateway `/model` commands, `select_llm` and
/// `get_system_info`: the models of every registered provider that listed them, plus the
/// providers that could not (with the outermost error message only, so request URLs and keys
/// inside lower error layers are not exposed).
pub(crate) async fn available_models(state: &AppState) -> ModelCatalog {
    let mut catalog = ModelCatalog::default();
    let router = state.llm_router.get();
    for pname in router.provider_names() {
        let Some(provider) = router.get_provider(pname) else {
            continue;
        };
        match provider.clone().available_models().await {
            Ok(models) => catalog.models.extend(
                models
                    .into_iter()
                    .map(|model| format!("{pname}:{}", model.id)),
            ),
            Err(error) => {
                tracing::warn!(provider = %pname, error = %error, "model catalog: provider did not list its models");
                catalog.unavailable_providers.push(UnavailableProvider {
                    provider: pname.to_string(),
                    error: error.to_string(),
                });
            }
        }
    }
    catalog.models.sort();
    catalog.models.dedup();
    catalog
        .unavailable_providers
        .sort_by(|a, b| a.provider.cmp(&b.provider));
    catalog
}

/// ダッシュボードのモデルセレクタ用: 既定モデルと各プロバイダの利用可能モデル一覧。
pub async fn model_choices(State(state): State<AppState>) -> Json<serde_json::Value> {
    let catalog = available_models(&state).await;
    Json(serde_json::json!({
        "default_model": state.default_model(),
        "choices": catalog.models,
        "unavailable_providers": catalog.unavailable_providers_json(),
    }))
}

#[cfg(test)]
mod slash_model_resolution_contract {
    use std::sync::Arc;

    use async_trait::async_trait;
    use opencrab_actions::{ModelAdminError, ModelAdministration, UnavailableProvider};
    use opencrab_llm::traits::{LlmProvider, ModelInfo};

    use super::available_models;
    use opencrab_actions::{resolve_exact_model, ModelResolutionError as ResolutionError};

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

    struct ListingModelCatalog;

    #[async_trait]
    impl LlmProvider for ListingModelCatalog {
        fn name(&self) -> &str {
            "listing"
        }

        async fn available_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
            Ok(vec![ModelInfo {
                id: "model-a".to_string(),
                name: "model-a".to_string(),
                context_window: 1000,
                supports_function_calling: true,
                supports_vision: false,
            }])
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

    fn state_with_failing_catalog(configured_model: Option<&str>) -> crate::AppState {
        let state = crate::test_app_state();
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::upsert_agent(
                &conn,
                &opencrab_db::queries::AgentRow {
                    agent_id: "agent-x".to_string(),
                    name: "Agent X".to_string(),
                    job_title: None,
                    organization: None,
                    image_url: None,
                    persona_name: "Agent X".to_string(),
                    personality: None,
                    instructions: String::new(),
                    model: configured_model.map(str::to_string),
                    reasoning_effort: None,
                    web_search: None,
                    metadata_json: None,
                },
            )
            .unwrap();
        }
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(Arc::new(FailingModelCatalog));
        state.llm_router.swap(router);
        state
    }

    /// D-1141: one provider that cannot list its models does not hide the others; it is
    /// reported with its error, and the REST list, `/model` and `select_llm` share the result.
    #[tokio::test]
    async fn provider_failure_leaves_the_other_providers_models_and_reports_it() {
        let state = state_with_failing_catalog(None);
        let mut router = opencrab_llm::router::LlmRouter::new();
        router.add_provider(Arc::new(FailingModelCatalog));
        router.add_provider(Arc::new(ListingModelCatalog));
        state.llm_router.swap(router);

        let catalog = available_models(&state).await;
        assert_eq!(catalog.models, vec!["listing:model-a".to_string()]);
        assert_eq!(
            catalog.unavailable_providers,
            vec![UnavailableProvider {
                provider: "failing".to_string(),
                error: "provider-secret-detail".to_string(),
            }]
        );

        let snapshot = state.list_models("agent-x").await.unwrap();
        assert_eq!(snapshot.models, catalog.models);
        assert_eq!(
            snapshot.unavailable_providers,
            catalog.unavailable_providers
        );

        let rest = super::model_choices(axum::extract::State(state.clone()))
            .await
            .0;
        assert_eq!(rest["choices"], serde_json::json!(["listing:model-a"]));
        assert_eq!(
            rest["unavailable_providers"],
            serde_json::json!([{"provider": "failing", "error": "provider-secret-detail"}])
        );

        opencrab_db::queries::upsert_model_pricing(
            &state.db.lock().unwrap(),
            &opencrab_db::queries::ModelPricingRow {
                provider: "listing".to_string(),
                model: "model-a".to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(8_000),
                cached_input_price_per_1m: None,
                cache_write_price_per_1m: None,
            },
        )
        .unwrap();
        let set = state.set_model("agent-x", "model-a").await.unwrap();
        assert_eq!(set.configured_model.as_deref(), Some("listing:model-a"));
    }

    #[tokio::test]
    async fn repeated_exact_set_succeeds_when_catalog_refresh_fails() {
        let state = state_with_failing_catalog(Some("failing:model-x"));

        let snapshot = state.set_model("agent-x", "failing:model-x").await.unwrap();

        assert_eq!(snapshot.models, Vec::<String>::new());
        assert_eq!(
            snapshot.configured_model.as_deref(),
            Some("failing:model-x")
        );
        assert_eq!(snapshot.current_model, "failing:model-x");
    }

    #[tokio::test]
    async fn repeated_reset_succeeds_when_catalog_refresh_fails() {
        let state = state_with_failing_catalog(None);

        let snapshot = state.reset_model("agent-x").await.unwrap();

        assert_eq!(snapshot.models, Vec::<String>::new());
        assert_eq!(snapshot.configured_model, None);
        assert_eq!(snapshot.current_model, state.default_model());
    }

    #[tokio::test]
    async fn models_of_an_unavailable_provider_are_not_selectable() {
        let state = state_with_failing_catalog(Some("failing:model-x"));

        assert_eq!(
            state.set_model("agent-x", "failing:model-y").await,
            Err(ModelAdminError::NotFound)
        );
        assert_eq!(
            state.set_model("agent-x", "model-x").await,
            Err(ModelAdminError::NotFound)
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
