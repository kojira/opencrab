use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Pricing information for a single model (per 1M tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub provider: String,
    pub model: String,
    /// Cost per 1M input tokens in USD.
    pub input_per_million: f64,
    /// Cost per 1M output tokens in USD.
    pub output_per_million: f64,
}

impl ModelPricing {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        input_per_million: f64,
        output_per_million: f64,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            input_per_million,
            output_per_million,
        }
    }

    /// Calculate cost for given token counts.
    pub fn calculate_cost(&self, input_tokens: u32, output_tokens: u32) -> f64 {
        let input_cost = (input_tokens as f64 / 1_000_000.0) * self.input_per_million;
        let output_cost = (output_tokens as f64 / 1_000_000.0) * self.output_per_million;
        input_cost + output_cost
    }
}

/// Registry of model pricing information.
#[derive(Debug, Clone)]
pub struct PricingRegistry {
    /// Map from "provider:model" to pricing.
    prices: HashMap<String, ModelPricing>,
}

impl PricingRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            prices: HashMap::new(),
        };
        registry.load_defaults();
        registry
    }

    /// Register pricing for a model.
    pub fn register(&mut self, pricing: ModelPricing) {
        let key = format!("{}:{}", pricing.provider, pricing.model);
        self.prices.insert(key, pricing);
    }

    /// Look up pricing for a given provider and model.
    pub fn get(&self, provider: &str, model: &str) -> Option<&ModelPricing> {
        let key = format!("{provider}:{model}");
        self.prices.get(&key)
    }

    /// Calculate cost for a usage record.
    pub fn calculate_cost(
        &self,
        provider: &str,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
    ) -> Option<f64> {
        self.get(provider, model)
            .map(|p| p.calculate_cost(input_tokens, output_tokens))
    }

    /// Load default pricing data for well-known models.
    fn load_defaults(&mut self) {
        let defaults = vec![
            // OpenAI
            ModelPricing::new("openai", "gpt-4o", 2.50, 10.00),
            ModelPricing::new("openai", "gpt-4o-mini", 0.15, 0.60),
            ModelPricing::new("openai", "gpt-4-turbo", 10.00, 30.00),
            ModelPricing::new("openai", "gpt-4", 30.00, 60.00),
            ModelPricing::new("openai", "gpt-3.5-turbo", 0.50, 1.50),
            ModelPricing::new("openai", "o1", 15.00, 60.00),
            ModelPricing::new("openai", "o1-mini", 3.00, 12.00),
            // Anthropic
            ModelPricing::new("anthropic", "claude-sonnet-4-20250514", 3.00, 15.00),
            ModelPricing::new("anthropic", "claude-3-5-sonnet-20241022", 3.00, 15.00),
            ModelPricing::new("anthropic", "claude-3-opus-20240229", 15.00, 75.00),
            ModelPricing::new("anthropic", "claude-3-haiku-20240307", 0.25, 1.25),
            // Google
            ModelPricing::new("google", "gemini-2.0-flash", 0.10, 0.40),
            ModelPricing::new("google", "gemini-1.5-pro", 1.25, 5.00),
            ModelPricing::new("google", "gemini-1.5-flash", 0.075, 0.30),
            // Ollama / llama.cpp (local, free)
            ModelPricing::new("ollama", "*", 0.0, 0.0),
            ModelPricing::new("llamacpp", "*", 0.0, 0.0),
        ];

        for pricing in defaults {
            self.register(pricing);
        }
    }
}

impl Default for PricingRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_cost() {
        let pricing = ModelPricing::new("openai", "gpt-4o", 2.5, 10.0);
        let cost = pricing.calculate_cost(1000, 500);
        let expected = (1000.0 / 1_000_000.0) * 2.5 + (500.0 / 1_000_000.0) * 10.0;
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected {expected}, got {cost}"
        );
        assert!((cost - 0.0075).abs() < 1e-12);
    }

    #[test]
    fn test_registry_defaults() {
        let registry = PricingRegistry::new();
        assert!(
            registry.get("openai", "gpt-4o").is_some(),
            "gpt-4o should be in default registry"
        );
    }

    #[test]
    fn test_registry_custom() {
        let mut registry = PricingRegistry::new();
        registry.register(ModelPricing::new("custom", "my-model", 1.0, 2.0));
        let pricing = registry.get("custom", "my-model");
        assert!(pricing.is_some());
        let pricing = pricing.unwrap();
        assert_eq!(pricing.provider, "custom");
        assert_eq!(pricing.model, "my-model");
        assert!((pricing.input_per_million - 1.0).abs() < f64::EPSILON);
        assert!((pricing.output_per_million - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_registry_calculate_cost() {
        let registry = PricingRegistry::new();
        let cost = registry.calculate_cost("openai", "gpt-4o", 1000, 500);
        assert!(cost.is_some());
        let cost = cost.unwrap();
        let expected = (1000.0 / 1_000_000.0) * 2.5 + (500.0 / 1_000_000.0) * 10.0;
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected {expected}, got {cost}"
        );
    }
}
