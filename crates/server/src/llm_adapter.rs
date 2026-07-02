use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;

use opencrab_core::{ChatRequest, ChatResponse, LlmClient};
use opencrab_llm::pricing::PricingRegistry;
use opencrab_llm::router::LlmRouter;

/// Configuration for metrics recording.
pub struct MetricsContext {
    pub db: opencrab_db::Db,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub pricing: PricingRegistry,
    /// Shared state: updated after each LLM call so actions can reference it.
    pub last_metrics_id: Arc<Mutex<Option<String>>>,
    /// Shared current purpose: actions (e.g. select_llm) can update this
    /// to tag subsequent LLM calls with the correct purpose.
    pub current_purpose: Arc<Mutex<String>>,
}

/// Adapter that wraps an `LlmRouter` and implements `LlmClient` so that
/// `SkillEngine` can use it directly.
///
/// Since the engine and the provider/router layer now share one canonical
/// message model (`opencrab-llm-types`), this adapter no longer converts
/// between two representations — it forwards the request to the router and,
/// optionally, records usage metrics to the DB.
pub struct LlmRouterAdapter {
    router: Arc<LlmRouter>,
    metrics_ctx: Option<MetricsContext>,
    agent_id: Option<String>,
}

impl LlmRouterAdapter {
    pub fn new(router: Arc<LlmRouter>) -> Self {
        Self {
            router,
            metrics_ctx: None,
            agent_id: None,
        }
    }

    pub fn with_metrics(mut self, ctx: MetricsContext) -> Self {
        self.metrics_ctx = Some(ctx);
        self
    }

    pub fn with_agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }
}

#[async_trait]
impl LlmClient for LlmRouterAdapter {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let model_requested = request.model.clone();
        let mut request = request;
        if self.agent_id.is_some() {
            request.agent_id = self.agent_id.clone();
        }

        let start = std::time::Instant::now();
        let response = self.router.chat_completion(request).await?;
        let latency_ms = start.elapsed().as_millis() as i64;

        // Record metrics to DB if context is available.
        if let Some(ref ctx) = self.metrics_ctx {
            let metrics_id = uuid::Uuid::new_v4().to_string();

            // Resolve provider and model from the alias.
            let (provider, model) = self
                .router
                .resolve_model(&model_requested)
                .unwrap_or_else(|_| ("unknown".to_string(), model_requested.clone()));

            let input_tokens = response.usage.prompt_tokens as i32;
            let output_tokens = response.usage.completion_tokens as i32;
            let total_tokens = response.usage.total_tokens as i32;

            let estimated_cost = ctx
                .pricing
                .calculate_cost(&provider, &model, input_tokens as u32, output_tokens as u32)
                .unwrap_or(0.0);

            let row = opencrab_db::queries::LlmMetricsRow {
                id: metrics_id.clone(),
                agent_id: ctx.agent_id.clone(),
                session_id: ctx.session_id.clone(),
                timestamp: Utc::now().to_rfc3339(),
                provider,
                model,
                purpose: ctx
                    .current_purpose
                    .lock()
                    .map(|p| p.clone())
                    .unwrap_or_else(|_| "conversation".to_string()),
                task_type: None,
                complexity: None,
                input_tokens,
                output_tokens,
                total_tokens,
                estimated_cost_usd: estimated_cost,
                latency_ms,
                time_to_first_token_ms: None,
            };

            if let Ok(conn) = ctx.db.lock() {
                if let Err(e) = opencrab_db::queries::insert_llm_metrics(&conn, &row) {
                    tracing::warn!(error = %e, "Failed to record LLM metrics");
                }
            }

            // Update shared last_metrics_id so actions can reference it.
            if let Ok(mut id) = ctx.last_metrics_id.lock() {
                *id = Some(metrics_id);
            }
        }

        Ok(response)
    }
}
