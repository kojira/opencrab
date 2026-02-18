use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::pricing::PricingRegistry;

/// A single usage record from an LLM call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub timestamp: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub latency_ms: u64,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregated statistics for a provider/model pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregatedStats {
    pub total_requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tokens: u64,
    pub total_latency_ms: u64,
    pub estimated_cost_usd: f64,
}

impl AggregatedStats {
    /// Average latency in milliseconds.
    pub fn avg_latency_ms(&self) -> f64 {
        if self.total_requests == 0 {
            return 0.0;
        }
        self.total_latency_ms as f64 / self.total_requests as f64
    }

    /// Success rate as a fraction from 0.0 to 1.0.
    pub fn success_rate(&self) -> f64 {
        if self.total_requests == 0 {
            return 0.0;
        }
        self.successful_requests as f64 / self.total_requests as f64
    }
}

/// Thread-safe metrics collector for LLM usage.
#[derive(Debug, Clone)]
pub struct MetricsCollector {
    records: Arc<Mutex<Vec<UsageRecord>>>,
    pricing: Arc<PricingRegistry>,
}

impl MetricsCollector {
    pub fn new(pricing: PricingRegistry) -> Self {
        Self {
            records: Arc::new(Mutex::new(Vec::new())),
            pricing: Arc::new(pricing),
        }
    }

    /// Record a usage event.
    pub fn record(&self, record: UsageRecord) {
        let mut records = self.records.lock().expect("metrics lock poisoned");
        records.push(record);
    }

    /// Record a successful completion call.
    pub fn record_success(
        &self,
        provider: &str,
        model: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        latency_ms: u64,
    ) {
        self.record(UsageRecord {
            timestamp: Utc::now(),
            provider: provider.to_string(),
            model: model.to_string(),
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            latency_ms,
            success: true,
            error: None,
        });
    }

    /// Record a failed completion call.
    pub fn record_failure(
        &self,
        provider: &str,
        model: &str,
        latency_ms: u64,
        error: &str,
    ) {
        self.record(UsageRecord {
            timestamp: Utc::now(),
            provider: provider.to_string(),
            model: model.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            latency_ms,
            success: false,
            error: Some(error.to_string()),
        });
    }

    /// Get all raw records.
    pub fn records(&self) -> Vec<UsageRecord> {
        let records = self.records.lock().expect("metrics lock poisoned");
        records.clone()
    }

    /// Get aggregated stats per provider.
    pub fn stats_by_provider(&self) -> HashMap<String, AggregatedStats> {
        let records = self.records.lock().expect("metrics lock poisoned");
        let mut map: HashMap<String, AggregatedStats> = HashMap::new();

        for rec in records.iter() {
            let stats = map.entry(rec.provider.clone()).or_default();
            Self::accumulate(stats, rec, &self.pricing);
        }

        map
    }

    /// Get aggregated stats per model (key = "provider:model").
    pub fn stats_by_model(&self) -> HashMap<String, AggregatedStats> {
        let records = self.records.lock().expect("metrics lock poisoned");
        let mut map: HashMap<String, AggregatedStats> = HashMap::new();

        for rec in records.iter() {
            let key = format!("{}:{}", rec.provider, rec.model);
            let stats = map.entry(key).or_default();
            Self::accumulate(stats, rec, &self.pricing);
        }

        map
    }

    /// Get total aggregated stats across all providers.
    pub fn total_stats(&self) -> AggregatedStats {
        let records = self.records.lock().expect("metrics lock poisoned");
        let mut stats = AggregatedStats::default();

        for rec in records.iter() {
            Self::accumulate(&mut stats, rec, &self.pricing);
        }

        stats
    }

    /// Clear all recorded metrics.
    pub fn clear(&self) {
        let mut records = self.records.lock().expect("metrics lock poisoned");
        records.clear();
    }

    fn accumulate(stats: &mut AggregatedStats, rec: &UsageRecord, pricing: &PricingRegistry) {
        stats.total_requests += 1;
        if rec.success {
            stats.successful_requests += 1;
        } else {
            stats.failed_requests += 1;
        }
        stats.total_prompt_tokens += rec.prompt_tokens as u64;
        stats.total_completion_tokens += rec.completion_tokens as u64;
        stats.total_tokens += rec.total_tokens as u64;
        stats.total_latency_ms += rec.latency_ms;

        if let Some(cost) =
            pricing.calculate_cost(&rec.provider, &rec.model, rec.prompt_tokens, rec.completion_tokens)
        {
            stats.estimated_cost_usd += cost;
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new(PricingRegistry::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> MetricsCollector {
        MetricsCollector::default()
    }

    #[test]
    fn test_record_success() {
        let mc = collector();
        mc.record_success("openai", "gpt-4o", 100, 50, 200);
        let records = mc.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].success);
        assert_eq!(records[0].provider, "openai");
        assert_eq!(records[0].model, "gpt-4o");
    }

    #[test]
    fn test_record_failure() {
        let mc = collector();
        mc.record_failure("openai", "gpt-4o", 150, "timeout");
        let records = mc.records();
        assert_eq!(records.len(), 1);
        assert!(!records[0].success);
        assert_eq!(records[0].error.as_deref(), Some("timeout"));
    }

    #[test]
    fn test_stats_by_provider() {
        let mc = collector();
        mc.record_success("openai", "gpt-4o", 100, 50, 200);
        mc.record_success("openai", "gpt-4o", 200, 100, 300);
        mc.record_success("anthropic", "claude-sonnet-4-20250514", 50, 25, 100);

        let stats = mc.stats_by_provider();
        assert_eq!(stats.len(), 2);
        assert!(stats.contains_key("openai"));
        assert!(stats.contains_key("anthropic"));
        assert_eq!(stats["openai"].total_requests, 2);
        assert_eq!(stats["anthropic"].total_requests, 1);
    }

    #[test]
    fn test_stats_by_model() {
        let mc = collector();
        mc.record_success("openai", "gpt-4o", 100, 50, 200);
        mc.record_success("openai", "gpt-4o-mini", 50, 25, 100);

        let stats = mc.stats_by_model();
        assert!(stats.contains_key("openai:gpt-4o"));
        assert!(stats.contains_key("openai:gpt-4o-mini"));
    }

    #[test]
    fn test_total_stats() {
        let mc = collector();
        mc.record_success("openai", "gpt-4o", 100, 50, 200);
        mc.record_success("openai", "gpt-4o", 200, 100, 300);
        mc.record_failure("anthropic", "claude-sonnet-4-20250514", 50, "error");

        let stats = mc.total_stats();
        assert_eq!(stats.total_requests, 3);
        assert_eq!(stats.successful_requests, 2);
        assert_eq!(stats.failed_requests, 1);
    }

    #[test]
    fn test_success_rate() {
        let stats = AggregatedStats {
            total_requests: 4,
            successful_requests: 3,
            failed_requests: 1,
            ..Default::default()
        };
        assert!((stats.success_rate() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_avg_latency() {
        let stats = AggregatedStats {
            total_requests: 4,
            total_latency_ms: 1000,
            ..Default::default()
        };
        assert!((stats.avg_latency_ms() - 250.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clear() {
        let mc = collector();
        mc.record_success("openai", "gpt-4o", 100, 50, 200);
        assert_eq!(mc.records().len(), 1);
        mc.clear();
        assert!(mc.records().is_empty());
    }
}
