use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

use opencrab_llm::providers::*;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;

// ---------- Config structs (match config/default.toml) ----------

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
}

#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_provider")]
    pub default_provider: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub aliases: HashMap<String, AliasConfig>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            default_model: "gpt-4o".to_string(),
            providers: HashMap::new(),
            fallback: FallbackConfig::default(),
            aliases: HashMap::new(),
        }
    }
}

fn default_provider() -> String {
    "openai".to_string()
}
fn default_model() -> String {
    "gpt-4o".to_string()
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub organization: String,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub site_url: String,
    #[serde(default)]
    pub default_model: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct FallbackConfig {
    #[serde(default)]
    pub chain: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AliasConfig {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct GatewayConfig {
    #[serde(default)]
    pub rest: RestGatewayConfig,
    #[serde(default)]
    pub discord: DiscordGatewayConfig,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct DiscordGatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub guild_ids: Vec<u64>,
    /// Discordメッセージに応答するエージェントのIDリスト
    #[serde(default)]
    pub agent_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RestGatewayConfig {
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for RestGatewayConfig {
    fn default() -> Self {
        Self { port: 8080 }
    }
}

fn default_port() -> u16 {
    8080
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
        }
    }
}

fn default_db_path() -> String {
    "data/opencrab.db".to_string()
}

// ---------- Config loading ----------

/// Load config from a TOML file, expanding `${VAR}` placeholders with env vars.
pub fn load_config(path: &str) -> Result<AppConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    let expanded = expand_env_vars(&raw);

    let config: AppConfig =
        toml::from_str(&expanded).with_context(|| "Failed to parse config TOML")?;

    Ok(config)
}

/// Replace `${VAR_NAME}` patterns with corresponding environment variable values.
/// Unknown variables are replaced with empty strings.
fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // Find all ${...} patterns and replace them
    loop {
        let start = match result.find("${") {
            Some(pos) => pos,
            None => break,
        };
        let end = match result[start..].find('}') {
            Some(pos) => start + pos,
            None => break,
        };
        let var_name = &result[start + 2..end];
        let value = std::env::var(var_name).unwrap_or_default();
        result = format!("{}{}{}", &result[..start], value, &result[end + 1..]);
    }
    result
}

// ---------- LLM Router builder ----------

/// Build an LlmRouter from the LLM config section.
/// Only providers with non-empty API keys (or local providers) are registered.
pub fn build_llm_router(config: &LlmConfig) -> Result<LlmRouter> {
    let mut router = LlmRouter::new();

    for (name, pconfig) in &config.providers {
        let provider: Option<Arc<dyn LlmProvider>> = match name.as_str() {
            "openai" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = OpenAiProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    if !pconfig.organization.is_empty() {
                        p = p.with_org_id(&pconfig.organization);
                    }
                    Some(Arc::new(p))
                }
            }
            "anthropic" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = AnthropicProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "google" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = GoogleProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "openrouter" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = OpenRouterProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    if !pconfig.app_name.is_empty() {
                        p = p.with_title(&pconfig.app_name);
                    }
                    if !pconfig.site_url.is_empty() {
                        p = p.with_referer(&pconfig.site_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "ollama" => {
                let mut p = OllamaProvider::new();
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            "llamacpp" => {
                let mut p = LlamaCppProvider::new();
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            other => {
                info!(provider = %other, "Unknown provider in config, skipping");
                None
            }
        };

        if let Some(p) = provider {
            router.add_provider(p);
        }
    }

    // Set default provider
    router.set_default_provider(&config.default_provider);

    // Set fallback chain (only include registered providers)
    let registered: Vec<String> = router.provider_names().iter().map(|s| s.to_string()).collect();
    let chain: Vec<String> = config
        .fallback
        .chain
        .iter()
        .filter(|name| registered.contains(name))
        .cloned()
        .collect();
    if !chain.is_empty() {
        router.set_fallback_chain(chain);
    }

    // Set model aliases
    for (alias, acfg) in &config.aliases {
        let target = format!("{}:{}", acfg.provider, acfg.model);
        router.add_model_mapping(alias, target);
    }

    info!(
        providers = ?router.provider_names(),
        "LLM router configured"
    );

    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars() {
        std::env::set_var("TEST_EXPAND_KEY", "hello123");
        let input = "api_key = \"${TEST_EXPAND_KEY}\"";
        let result = expand_env_vars(input);
        assert_eq!(result, "api_key = \"hello123\"");
        std::env::remove_var("TEST_EXPAND_KEY");
    }

    #[test]
    fn test_expand_env_vars_missing() {
        let input = "api_key = \"${NONEXISTENT_VAR_12345}\"";
        let result = expand_env_vars(input);
        assert_eq!(result, "api_key = \"\"");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        std::env::set_var("TEST_A", "aaa");
        std::env::set_var("TEST_B", "bbb");
        let input = "${TEST_A} and ${TEST_B}";
        let result = expand_env_vars(input);
        assert_eq!(result, "aaa and bbb");
        std::env::remove_var("TEST_A");
        std::env::remove_var("TEST_B");
    }

    #[test]
    fn test_default_config() {
        let toml_str = "";
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.database.path, "data/opencrab.db");
        assert_eq!(config.gateway.rest.port, 8080);
        assert_eq!(config.llm.default_provider, "openai");
    }

    #[test]
    fn test_build_router_empty_keys() {
        let config = LlmConfig::default();
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().is_empty());
    }

    #[test]
    fn test_build_router_with_openrouter() {
        let mut providers = HashMap::new();
        providers.insert(
            "openrouter".to_string(),
            ProviderConfig {
                api_key: "sk-test-key".to_string(),
                app_name: "TestApp".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "openrouter".to_string(),
            ..Default::default()
        };
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().contains(&"openrouter"));
    }
}
