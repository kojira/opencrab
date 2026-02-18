use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures::stream::BoxStream;
use tracing::{debug, info, warn};

use crate::message::{ChatRequest, ChatResponse, ChatStreamDelta};
use crate::metrics::MetricsCollector;
use crate::traits::LlmProvider;

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

    /// Register a provider under its name.
    pub fn add_provider(&mut self, provider: Arc<dyn LlmProvider>) {
        let name = provider.name().to_string();
        info!(provider = %name, "Registered LLM provider");
        self.providers.insert(name, provider);
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
    pub async fn chat_completion(&self, mut request: ChatRequest) -> Result<ChatResponse> {
        let (provider_name, model_name) = self.resolve_model(&request.model)?;
        request.model = model_name;

        debug!(provider = %provider_name, model = %request.model, "Routing chat completion");

        // Try the resolved provider first
        if let Some(provider) = self.providers.get(&provider_name) {
            let start = std::time::Instant::now();
            match provider.chat_completion(request.clone()).await {
                Ok(response) => {
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_success(
                            &provider_name,
                            &response.model,
                            response.usage.prompt_tokens,
                            response.usage.completion_tokens,
                            start.elapsed().as_millis() as u64,
                        );
                    }
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        provider = %provider_name,
                        error = %e,
                        "Primary provider failed, trying fallback chain"
                    );
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_failure(
                            &provider_name,
                            &request.model,
                            start.elapsed().as_millis() as u64,
                            &e.to_string(),
                        );
                    }
                }
            }
        } else {
            warn!(provider = %provider_name, "Provider not found, trying fallback chain");
        }

        // Try fallback chain
        for fallback_name in &self.fallback_chain {
            if fallback_name == &provider_name {
                continue; // Skip the provider we already tried
            }

            if let Some(provider) = self.providers.get(fallback_name) {
                debug!(provider = %fallback_name, "Trying fallback provider");
                let start = std::time::Instant::now();
                match provider.chat_completion(request.clone()).await {
                    Ok(response) => {
                        info!(provider = %fallback_name, "Fallback provider succeeded");
                        if let Some(ref metrics) = self.metrics {
                            metrics.record_success(
                                fallback_name,
                                &response.model,
                                response.usage.prompt_tokens,
                                response.usage.completion_tokens,
                                start.elapsed().as_millis() as u64,
                            );
                        }
                        return Ok(response);
                    }
                    Err(e) => {
                        warn!(
                            provider = %fallback_name,
                            error = %e,
                            "Fallback provider failed"
                        );
                        if let Some(ref metrics) = self.metrics {
                            metrics.record_failure(
                                fallback_name,
                                &request.model,
                                start.elapsed().as_millis() as u64,
                                &e.to_string(),
                            );
                        }
                    }
                }
            }
        }

        anyhow::bail!(
            "All providers failed for model '{}'. Tried: {} + fallback chain {:?}",
            request.model,
            provider_name,
            self.fallback_chain
        )
    }

    /// Route a streaming chat completion request.
    pub async fn chat_completion_stream(
        &self,
        mut request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        let (provider_name, model_name) = self.resolve_model(&request.model)?;
        request.model = model_name;

        debug!(provider = %provider_name, model = %request.model, "Routing streaming chat completion");

        // Try resolved provider first
        if let Some(provider) = self.providers.get(&provider_name) {
            match provider.chat_completion_stream(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    warn!(
                        provider = %provider_name,
                        error = %e,
                        "Primary provider stream failed, trying fallback chain"
                    );
                }
            }
        }

        // Try fallback chain
        for fallback_name in &self.fallback_chain {
            if fallback_name == &provider_name {
                continue;
            }

            if let Some(provider) = self.providers.get(fallback_name) {
                match provider.chat_completion_stream(request.clone()).await {
                    Ok(stream) => {
                        info!(provider = %fallback_name, "Fallback provider stream succeeded");
                        return Ok(stream);
                    }
                    Err(e) => {
                        warn!(
                            provider = %fallback_name,
                            error = %e,
                            "Fallback provider stream failed"
                        );
                    }
                }
            }
        }

        anyhow::bail!(
            "All providers failed for streaming model '{}'. Tried: {} + fallback chain {:?}",
            request.model,
            provider_name,
            self.fallback_chain
        )
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
mod tests {
    use super::*;
    use crate::message::*;
    use crate::traits::LlmProvider;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockProvider {
        provider_name: String,
        should_fail: AtomicBool,
    }

    impl MockProvider {
        fn new(name: &str, should_fail: bool) -> Self {
            Self {
                provider_name: name.to_string(),
                should_fail: AtomicBool::new(should_fail),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }
        async fn available_models(&self) -> anyhow::Result<Vec<crate::traits::ModelInfo>> {
            Ok(vec![])
        }
        async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            if self.should_fail.load(Ordering::SeqCst) {
                anyhow::bail!("mock failure");
            }
            Ok(ChatResponse {
                id: "resp-1".to_string(),
                model: request.model,
                choices: vec![Choice {
                    index: 0,
                    message: Message::assistant("mock response"),
                    finish_reason: Some(FinishReason::Stop),
                }],
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                },
                created: 0,
            })
        }
    }

    #[test]
    fn test_resolve_model_default() {
        let mut router = LlmRouter::new();
        router.set_default_provider("openai");
        let (provider, model) = router.resolve_model("gpt-4o").unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_resolve_model_explicit() {
        let router = LlmRouter::new();
        let (provider, model) = router.resolve_model("anthropic:claude").unwrap();
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude");
    }

    #[test]
    fn test_resolve_model_alias() {
        let mut router = LlmRouter::new();
        router.add_model_mapping("best", "openai:gpt-4o");
        let (provider, model) = router.resolve_model("best").unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_resolve_model_no_default_error() {
        let router = LlmRouter::new();
        let result = router.resolve_model("gpt-4o");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_provider_success() {
        let mut router = LlmRouter::new();
        router.add_provider(Arc::new(MockProvider::new("openai", false)));
        router.set_default_provider("openai");

        let request = ChatRequest::new("gpt-4o", vec![Message::user("hello")]);
        let response = router.chat_completion(request).await;
        assert!(response.is_ok());
        let response = response.unwrap();
        assert_eq!(response.first_text(), Some("mock response"));
    }

    #[tokio::test]
    async fn test_provider_fallback() {
        let mut router = LlmRouter::new();
        router.add_provider(Arc::new(MockProvider::new("primary", true)));
        router.add_provider(Arc::new(MockProvider::new("fallback", false)));
        router.set_default_provider("primary");
        router.set_fallback_chain(vec!["fallback".to_string()]);

        let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
        let response = router.chat_completion(request).await;
        assert!(response.is_ok());
        assert_eq!(response.unwrap().first_text(), Some("mock response"));
    }

    #[tokio::test]
    async fn test_all_providers_fail() {
        let mut router = LlmRouter::new();
        router.add_provider(Arc::new(MockProvider::new("primary", true)));
        router.add_provider(Arc::new(MockProvider::new("fallback", true)));
        router.set_default_provider("primary");
        router.set_fallback_chain(vec!["fallback".to_string()]);

        let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
        let response = router.chat_completion(request).await;
        assert!(response.is_err());
    }

    #[tokio::test]
    async fn test_skip_already_tried() {
        let mut router = LlmRouter::new();
        router.add_provider(Arc::new(MockProvider::new("primary", true)));
        router.set_default_provider("primary");
        // Include the primary in the fallback chain; it should be skipped
        router.set_fallback_chain(vec!["primary".to_string()]);

        let request = ChatRequest::new("some-model", vec![Message::user("hello")]);
        let response = router.chat_completion(request).await;
        assert!(response.is_err());
    }

    #[test]
    fn test_provider_names() {
        let mut router = LlmRouter::new();
        router.add_provider(Arc::new(MockProvider::new("openai", false)));
        router.add_provider(Arc::new(MockProvider::new("anthropic", false)));

        let names = router.provider_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"anthropic"));
    }
}
