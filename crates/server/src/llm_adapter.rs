use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;

use opencrab_core::{ChatRequest, ChatResponse, LlmClient, LlmExchange};

/// Configuration for metrics recording.
pub struct MetricsContext {
    pub db: opencrab_db::Db,
    pub agent_id: String,
    pub session_id: Option<String>,
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
    /// ホットスワップ対応の共有ハンドル。リクエストごとに `get()` で
    /// その時点のルーターを取るため、ダッシュボードでのプロバイダー
    /// 設定変更が長寿命のアダプタ（Discord ループ等）にも反映される。
    router: crate::SharedLlmRouter,
    metrics_ctx: Option<MetricsContext>,
    agent_id: Option<String>,
}

impl LlmRouterAdapter {
    pub fn new(router: crate::SharedLlmRouter) -> Self {
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
        Ok(self.chat_with_history(request).await?.response)
    }

    async fn chat_with_history(&self, request: ChatRequest) -> Result<LlmExchange> {
        let model_requested = request.model.clone();
        let mut request = request;
        if self.agent_id.is_some() {
            request.agent_id = self.agent_id.clone();
        }

        let start = std::time::Instant::now();
        let router = self.router.get();
        let exchange = router.chat_completion_with_history(request).await?;
        let latency_ms = start.elapsed().as_millis() as i64;

        // Record metrics to DB if context is available.
        if let Some(ref ctx) = self.metrics_ctx {
            let metrics_id = uuid::Uuid::new_v4().to_string();

            // Resolve provider and model from the alias.
            let (provider, model) = router
                .resolve_model(&model_requested)
                .unwrap_or_else(|_| ("unknown".to_string(), model_requested.clone()));

            let input_tokens = exchange.response.usage.prompt_tokens as i32;
            let output_tokens = exchange.response.usage.completion_tokens as i32;
            let total_tokens = exchange.response.usage.total_tokens as i32;

            let usage = &exchange.response.usage;
            let tokens = opencrab_db::queries::BilledTokens {
                input_tokens: usage.prompt_tokens as i64,
                output_tokens: usage.completion_tokens as i64,
                cache_read_tokens: usage.cache_read_input_tokens as i64,
                cache_write_tokens: usage.cache_creation_input_tokens as i64,
                cache_included_in_input: cache_included_in_input(&provider),
            };
            // 単価は `model_pricing`（DB）だけが出所。未登録なら 0 を記録し warn で見えるようにする。
            let pricing = ctx.db.lock().ok().and_then(|conn| {
                opencrab_db::queries::get_model_pricing(&conn, &provider, &model)
                    .ok()
                    .flatten()
            });
            let estimated_cost = match pricing {
                Some(p) => p.cost_usd(tokens),
                None => {
                    tracing::warn!(%provider, %model, "no model_pricing row; usage cost recorded as 0");
                    0.0
                }
            };

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

        Ok(exchange)
    }
}

/// Whether the provider's `prompt_tokens` already contains the cache read/write tokens.
///
/// OpenAI Responses-style providers (`chatgpt`, `codex`) report `input_tokens` including
/// `input_tokens_details.cached_tokens`, and hermit reports uncached + read + write as its prompt
/// count. Anthropic-style usage (anthropic, cursor) reports cache reads and writes separately from
/// the uncached input.
pub fn cache_included_in_input(provider: &str) -> bool {
    matches!(provider, "chatgpt" | "codex" | "hermit")
}
