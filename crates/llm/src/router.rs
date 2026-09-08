use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures::stream::BoxStream;
use tokio::time::Duration;
use tracing::{debug, info, warn};

use crate::message::{ChatRequest, ChatResponse, ChatStreamDelta};
use crate::metrics::MetricsCollector;
use crate::traits::LlmProvider;

/// Maximum number of retry attempts per provider.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (doubles each retry: 1s, 2s, 4s).
const BACKOFF_BASE_MS: u64 = 1000;

/// LLM Router for dynamic provider switching with fallback chains.
///
/// The router manages multiple LLM providers and supports:
/// - Named provider lookup
/// - Default provider selection
/// - Fallback chains (try providers in order until one succeeds)
/// - Model aliasing (map user-facing names to provider-specific models)
pub struct LlmRouter {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    default_provider: Option<String>,
    fallback_chain: Vec<String>,
    /// Maps alias names to "provider:model" strings.
    model_mapping: HashMap<String, String>,
    metrics: Option<MetricsCollector>,
}

impl LlmRouter {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider: None,
            fallback_chain: Vec::new(),
            model_mapping: HashMap::new(),
            metrics: None,
        }
    }

    /// Register a provider under an explicit routing name.
    ///
    /// The `name` given here — **not** `provider.name()` — is the key that
    /// `provider:model` specs resolve against. Keeping the routing key at this
    /// single caller-supplied assignment point is what stops it from silently
    /// diverging from the config section key: the router never reads
    /// `provider.name()` to decide routing, so the name a caller registers under
    /// and the name a spec resolves to are the same string by construction.
    /// `provider.name()` is only a display label for telemetry.
    pub fn register_provider(&mut self, name: impl Into<String>, provider: Arc<dyn LlmProvider>) {
        let name = name.into();
        info!(provider = %name, "Registered LLM provider");
        self.providers.insert(name, provider);
    }

    /// Register a provider under its own reported [`LlmProvider::name`].
    ///
    /// Convenience for callers (mostly tests) that want the provider's
    /// self-reported name as the routing key. Production wiring uses
    /// [`Self::register_provider`] with the config section key instead.
    pub fn add_provider(&mut self, provider: Arc<dyn LlmProvider>) {
        let name = provider.name().to_string();
        self.register_provider(name, provider);
    }

    /// Set the default provider name.
    pub fn set_default_provider(&mut self, name: impl Into<String>) {
        self.default_provider = Some(name.into());
    }

    /// Set the fallback chain (ordered list of provider names).
    pub fn set_fallback_chain(&mut self, chain: Vec<String>) {
        self.fallback_chain = chain;
    }

    /// Add a model alias mapping.
    /// The target should be in the format "provider:model".
    pub fn add_model_mapping(&mut self, alias: impl Into<String>, target: impl Into<String>) {
        self.model_mapping.insert(alias.into(), target.into());
    }

    /// Attach a metrics collector to the router.
    pub fn set_metrics(&mut self, metrics: MetricsCollector) {
        self.metrics = Some(metrics);
    }

    /// Get a provider by name.
    pub fn get_provider(&self, name: &str) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(name)
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<&Arc<dyn LlmProvider>> {
        self.default_provider
            .as_ref()
            .and_then(|name| self.providers.get(name))
    }

    /// List all registered provider names.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// エージェントに見せてよいモデル指定。登録済みプロバイダへ解決できるエイリアスと、
    /// その `provider:model` 先だけを返す。未登録プロバイダを指すエイリアスは出さない。
    pub fn configured_model_choices(&self) -> Vec<String> {
        let mut choices = Vec::new();
        for (alias, target) in &self.model_mapping {
            if let Ok((provider, _)) = self.parse_provider_model(target) {
                if self.providers.contains_key(&provider) {
                    choices.push(alias.clone());
                    if !choices.iter().any(|c| c == target) {
                        choices.push(target.clone());
                    }
                }
            }
        }
        choices.sort();
        choices.dedup();
        choices
    }

    /// `spec`（エイリアスまたは `provider:model`）の解決先プロバイダが登録済みか。
    /// 未登録なら理由つきで拒否する（実行前の fail-loud）。
    pub fn ensure_model_configured(&self, spec: &str) -> Result<()> {
        let spec = spec.trim();
        if spec.is_empty() {
            anyhow::bail!("model spec is empty");
        }
        let (provider, _model) = self.resolve_model(spec)?;
        if self.providers.contains_key(&provider) {
            return Ok(());
        }
        let mut registered: Vec<&str> = self.provider_names();
        registered.sort_unstable();
        anyhow::bail!(
            "未登録の LLM プロバイダ '{provider}' は使えません（指定: {spec}）。構成済み: [{}]",
            registered.join(", ")
        )
    }

    /// Resolve a model alias.
    /// Returns (provider_name, model_name).
    /// If the input contains ":", it's treated as "provider:model".
    /// If it's a known alias, it's resolved from the mapping.
    /// Otherwise, the default provider is used.
    pub fn resolve_model(&self, model_or_alias: &str) -> Result<(String, String)> {
        // Check alias mapping first
        if let Some(target) = self.model_mapping.get(model_or_alias) {
            return self.parse_provider_model(target);
        }

        // Check for "provider:model" format
        if model_or_alias.contains(':') {
            return self.parse_provider_model(model_or_alias);
        }

        // Use default provider
        if let Some(ref default) = self.default_provider {
            Ok((default.clone(), model_or_alias.to_string()))
        } else {
            anyhow::bail!(
                "No default provider set and model '{}' is not in provider:model format",
                model_or_alias
            );
        }
    }

    /// #676: この `model`（alias / `provider:model`）を捌くプロバイダが `max_tokens`
    /// を backend へ送るか。出力上限のモデル登録を要求すべきか（＝送るなら要求）の判断に使う。
    ///
    /// 解決先のプロバイダ自身の能力宣言（[`LlmProvider::sends_max_output_tokens`]）を返す。
    /// core 側で provider 名や type 文字列を突き合わせない（条件1）。解決失敗 / 未登録
    /// プロバイダは **`true`（送る＝登録必須側）** に倒す（新規や未知が黙って素通りしない）。
    pub fn sends_max_output_tokens(&self, model_or_alias: &str) -> bool {
        match self.resolve_model(model_or_alias) {
            Ok((provider_name, _)) => self
                .providers
                .get(&provider_name)
                .map(|p| p.sends_max_output_tokens())
                .unwrap_or(true),
            Err(_) => true,
        }
    }

    fn parse_provider_model(&self, s: &str) -> Result<(String, String)> {
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid provider:model format: '{}'", s);
        }
        Ok((parts[0].to_string(), parts[1].to_string()))
    }

    /// Route a chat completion request to the appropriate provider.
    ///
    /// Resolution order:
    /// 1. Resolve the model (alias -> provider:model)
    /// 2. Send to that provider
    /// 3. On failure, try the fallback chain
    pub async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        Ok(self.chat_completion_with_history(request).await?.response)
    }

    /// Route a chat completion while preserving provider-executed tool history.
    pub async fn chat_completion_with_history(
        &self,
        mut request: ChatRequest,
    ) -> Result<opencrab_llm_types::LlmExchange> {
        let (provider_name, model_name) = self.resolve_model(&request.model)?;
        request.model = model_name;

        debug!(provider = %provider_name, model = %request.model, "Routing chat completion");

        // 試した全プロバイダのエラーを保持する。last_error だけだと、primary
        // （例: codex）が失敗して fallback（例: ollama）も失敗したとき、最後の
        // fallback のエラーで primary のエラーが上書きされて消える。原因究明に
        // 必要なのは大抵 primary のエラーなので、全て残して要約文へ畳み込む。
        // 型付き LlmError の downcast（#35）は最後のエラーを chain の根に残して生かす。
        let mut errors: Vec<(String, anyhow::Error)> = Vec::new();

        // Try the resolved provider first (with retries)
        if let Some(provider) = self.providers.get(&provider_name) {
            match self
                .try_provider_with_retry(provider, &provider_name, &request)
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    warn!(
                        provider = %provider_name,
                        error = format!("{e:#}"),
                        "Primary provider failed after {MAX_RETRIES} attempts, trying fallback chain"
                    );
                    errors.push((provider_name.clone(), e));
                }
            }
        } else {
            warn!(provider = %provider_name, "Provider not found, trying fallback chain");
        }

        // Try fallback chain (each with retries)
        for fallback_name in &self.fallback_chain {
            if fallback_name == &provider_name {
                continue; // Skip the provider we already tried
            }

            if let Some(provider) = self.providers.get(fallback_name) {
                debug!(provider = %fallback_name, "Trying fallback provider");
                match self
                    .try_provider_with_retry(provider, fallback_name, &request)
                    .await
                {
                    Ok(response) => {
                        info!(provider = %fallback_name, "Fallback provider succeeded");
                        return Ok(response);
                    }
                    Err(e) => {
                        warn!(
                            provider = %fallback_name,
                            error = format!("{e:#}"),
                            "Fallback provider failed after {MAX_RETRIES} attempts"
                        );
                        errors.push((fallback_name.clone(), e));
                    }
                }
            }
        }

        // 各プロバイダの完全なエラーチェーン（{:#}）を要約文へ畳み込む。こうすると
        // 表示側が Display（{})でも全プロバイダの原因が見え、生エラーが消えない。
        let detail = errors
            .iter()
            .map(|(name, e)| format!("  [{name}] {e:#}"))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = if errors.is_empty() {
            format!(
                "No providers available for model '{}'. Resolved provider: {}, fallback chain: {:?}",
                request.model, provider_name, self.fallback_chain
            )
        } else {
            format!(
                "All providers failed for model '{}' ({} tried):\n{}",
                request.model,
                errors.len(),
                detail
            )
        };
        // 最後のエラーを chain の根に残す（型付き LlmError の downcast を生かす — #35）。
        // 要約文には全プロバイダのエラーが既に含まれる。
        Err(match errors.pop() {
            Some((_, e)) => e.context(summary),
            None => anyhow::anyhow!(summary),
        })
    }

    /// Route a streaming chat completion request.
    pub async fn chat_completion_stream(
        &self,
        mut request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        let (provider_name, model_name) = self.resolve_model(&request.model)?;
        request.model = model_name;

        debug!(provider = %provider_name, model = %request.model, "Routing streaming chat completion");

        let mut last_error: Option<anyhow::Error> = None;

        // Try resolved provider first (with retries)
        if let Some(provider) = self.providers.get(&provider_name) {
            match self
                .try_provider_stream_with_retry(provider, &provider_name, &request)
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    warn!(
                        provider = %provider_name,
                        error = %e,
                        "Primary provider stream failed after {MAX_RETRIES} attempts, trying fallback chain"
                    );
                    last_error = Some(e);
                }
            }
        }

        // Try fallback chain (each with retries)
        for fallback_name in &self.fallback_chain {
            if fallback_name == &provider_name {
                continue;
            }

            if let Some(provider) = self.providers.get(fallback_name) {
                match self
                    .try_provider_stream_with_retry(provider, fallback_name, &request)
                    .await
                {
                    Ok(stream) => {
                        info!(provider = %fallback_name, "Fallback provider stream succeeded");
                        return Ok(stream);
                    }
                    Err(e) => {
                        warn!(
                            provider = %fallback_name,
                            error = %e,
                            "Fallback provider stream failed after {MAX_RETRIES} attempts"
                        );
                        last_error = Some(e);
                    }
                }
            }
        }

        let summary = format!(
            "All providers failed for streaming model '{}'. Tried: {} + fallback chain {:?}",
            request.model, provider_name, self.fallback_chain
        );
        Err(match last_error {
            Some(e) => e.context(summary),
            None => anyhow::anyhow!(summary),
        })
    }

    /// Returns true if the error represents a non-retryable client-side HTTP error.
    ///
    /// Retry policy:
    ///   - Retryable:     429 (rate-limit), 5xx (transient server errors),
    ///     ステータス不明のエラー（ネットワーク・サブプロセス等）
    ///   - Non-retryable: other 4xx (permanent client errors — retrying won't help)
    ///
    /// 分類は型付き [`LlmError`] の downcast で行う（anyhow は context チェーンを
    /// 遡って downcast する）。Display 文字列の部分一致には依存しない（#35）。
    fn is_non_retryable_error(error: &anyhow::Error) -> bool {
        match error.downcast_ref::<crate::LlmError>() {
            Some(llm_error) => llm_error.is_non_retryable(),
            // ステータスを運ばないエラーは retryable（従来挙動を維持）
            None => false,
        }
    }

    /// Try a provider with exponential backoff retry (up to MAX_RETRIES attempts).
    ///
    /// Non-retryable 4xx errors are returned immediately without further attempts.
    async fn try_provider_with_retry(
        &self,
        provider: &Arc<dyn LlmProvider>,
        provider_name: &str,
        request: &ChatRequest,
    ) -> Result<opencrab_llm_types::LlmExchange> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(BACKOFF_BASE_MS * 2u64.pow(attempt - 1));
                warn!(
                    provider = %provider_name,
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    "Retrying after backoff"
                );
                tokio::time::sleep(delay).await;
            }

            let start = std::time::Instant::now();
            match provider.chat_completion_with_history(request.clone()).await {
                Ok(exchange) => {
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_success(
                            provider_name,
                            &exchange.response.model,
                            exchange.response.usage.prompt_tokens,
                            exchange.response.usage.completion_tokens,
                            start.elapsed().as_millis() as u64,
                        );
                    }
                    return Ok(exchange);
                }
                Err(e) => {
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_failure(
                            provider_name,
                            &request.model,
                            start.elapsed().as_millis() as u64,
                            &e.to_string(),
                        );
                    }
                    warn!(
                        provider = %provider_name,
                        attempt = attempt + 1,
                        error = %e,
                        "Provider attempt failed"
                    );
                    if Self::is_non_retryable_error(&e) {
                        warn!(
                            provider = %provider_name,
                            error = %e,
                            "Non-retryable client error (4xx), aborting retry"
                        );
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }

    /// Try a streaming provider with exponential backoff retry (up to MAX_RETRIES attempts).
    ///
    /// Non-retryable 4xx errors are returned immediately without further attempts.
    async fn try_provider_stream_with_retry(
        &self,
        provider: &Arc<dyn LlmProvider>,
        provider_name: &str,
        request: &ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(BACKOFF_BASE_MS * 2u64.pow(attempt - 1));
                warn!(
                    provider = %provider_name,
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    "Retrying stream after backoff"
                );
                tokio::time::sleep(delay).await;
            }

            match provider.chat_completion_stream(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    warn!(
                        provider = %provider_name,
                        attempt = attempt + 1,
                        error = %e,
                        "Provider stream attempt failed"
                    );
                    if Self::is_non_retryable_error(&e) {
                        warn!(
                            provider = %provider_name,
                            error = %e,
                            "Non-retryable client error (4xx), aborting stream retry"
                        );
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }

    /// Run health checks on all registered providers.
    pub async fn health_check_all(&self) -> HashMap<String, bool> {
        let mut results = HashMap::new();
        for (name, provider) in &self.providers {
            let healthy = provider.health_check().await.unwrap_or(false);
            results.insert(name.clone(), healthy);
        }
        results
    }
}

impl Default for LlmRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LlmRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmRouter")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default_provider", &self.default_provider)
            .field("fallback_chain", &self.fallback_chain)
            .field("model_mapping", &self.model_mapping)
            .finish()
    }
}

#[cfg(test)]
mod tests;
