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
    pub agent: AgentConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub tools: opencrab_actions::tools::ToolsConfig,
    #[serde(default)]
    pub evaluator: EvaluatorConfig,
    /// スリープ時スキル棚卸し（自己 curation ループ）。
    #[serde(default)]
    pub skill_consolidation: SkillConsolidationConfig,
    /// VC 対話（STT/TTS）。既定は無効。
    #[serde(default)]
    pub voice: opencrab_voice::VoiceConfig,
}

/// スリープ時スキル棚卸しの設定（design-sleep-skill-consolidation.md §10）。
#[derive(Debug, Deserialize, Clone)]
pub struct SkillConsolidationConfig {
    /// ループ全体の on/off。
    #[serde(default = "default_sc_enabled")]
    pub enabled: bool,
    /// 発火する新規活動（未処理セッション）数 N。
    #[serde(default = "default_sc_trigger")]
    pub trigger_new_sessions: i64,
    /// 保険トリガの時間キャップ（時間）。
    #[serde(default = "default_sc_time_cap")]
    pub time_cap_hours: i64,
    /// 最短間隔フロア（秒）。
    #[serde(default = "default_sc_min_interval")]
    pub min_interval_secs: i64,
    /// 棚卸しパケットに含める archived スキル数（再検討用）。
    #[serde(default = "default_sc_include_archived")]
    pub include_archived_in_review: i64,
}

impl Default for SkillConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: default_sc_enabled(),
            trigger_new_sessions: default_sc_trigger(),
            time_cap_hours: default_sc_time_cap(),
            min_interval_secs: default_sc_min_interval(),
            include_archived_in_review: default_sc_include_archived(),
        }
    }
}

fn default_sc_enabled() -> bool {
    // 設計 doc の既定は true だが、LLM を消費する自律ループのため安全側に倒して
    // opt-in（既定 false）とする。運営者が config で明示的に有効化する。
    false
}
fn default_sc_trigger() -> i64 {
    10
}
fn default_sc_time_cap() -> i64 {
    24
}
fn default_sc_min_interval() -> i64 {
    3600
}
fn default_sc_include_archived() -> i64 {
    3
}

/// verify 段（evaluator）の設定。
///
/// active タスクに contract（受け入れ条件）があるセッションの run 終了時、
/// 新しい context の LLM 呼び出しで rubric 評価し、結果を session_logs と
/// タスク台帳に記録する（record-only — エージェントは次ターンで gaps を見て
/// 自己修正する）。
#[derive(Debug, Deserialize, Clone)]
pub struct EvaluatorConfig {
    /// verify 段を有効にするか。
    #[serde(default = "default_evaluator_enabled")]
    pub enabled: bool,
    /// 合格スコア閾値 (0.0-1.0)。
    #[serde(default = "default_evaluator_threshold")]
    pub threshold: f64,
    /// 評価に使うモデル（省略時はそのエージェントの実効モデル）。
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        Self {
            enabled: default_evaluator_enabled(),
            threshold: default_evaluator_threshold(),
            model: None,
        }
    }
}

fn default_evaluator_enabled() -> bool {
    true
}
fn default_evaluator_threshold() -> f64 {
    0.7
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_workspace_path")]
    pub workspace_path: String,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_heartbeat_enabled")]
    pub heartbeat_enabled: bool,
    #[serde(default = "default_max_workspace_size")]
    pub max_workspace_size_mb: u64,
    /// ループ再起動 v1（#52）: depth 0 の run が反復上限で停止し、セッションに
    /// active タスクが残っている場合に、1回だけクリーンな context で自動再実行する。
    /// セッションロックを run1+verify+run2 の間保持し続けるため、既定は無効。
    #[serde(default)]
    pub loop_restart_enabled: bool,
    /// メモリインデックスのアイドル時メンテナンス（増分ビルドの取りこぼし回収 /
    /// キーワードバックフィル / 月次ロールアップ）。既定 true — 増分ビルドの費用は
    /// post-run トリガーで既に受容済みで、純増は一時的なバックフィルと月1回程度の
    /// ロールアップのみ。
    #[serde(default = "default_memory_maintenance_enabled")]
    pub memory_maintenance_enabled: bool,
    /// メンテナンス tick の間隔（秒）。無処理 tick は SQL 数本で終わる。
    #[serde(default = "default_memory_maintenance_interval")]
    pub memory_maintenance_interval_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            workspace_path: default_workspace_path(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            heartbeat_enabled: default_heartbeat_enabled(),
            max_workspace_size_mb: default_max_workspace_size(),
            loop_restart_enabled: false,
            memory_maintenance_enabled: default_memory_maintenance_enabled(),
            memory_maintenance_interval_secs: default_memory_maintenance_interval(),
        }
    }
}

fn default_memory_maintenance_enabled() -> bool {
    true
}
fn default_memory_maintenance_interval() -> u64 {
    600
}

fn default_workspace_path() -> String {
    "data/agents/{agent_id}/workspace".to_string()
}
fn default_heartbeat_interval() -> u64 {
    29
}
fn default_max_workspace_size() -> u64 {
    100
}
fn default_heartbeat_enabled() -> bool {
    false
}

#[derive(Debug, Deserialize, Clone)]
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
    /// 会話コンパクション比率: context_window のうち会話履歴に使う割合 (0.0-1.0)。
    #[serde(default = "default_compaction_ratio")]
    pub compaction_ratio: f64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            default_model: "gpt-4o".to_string(),
            providers: HashMap::new(),
            fallback: FallbackConfig::default(),
            aliases: HashMap::new(),
            compaction_ratio: default_compaction_ratio(),
        }
    }
}

fn default_compaction_ratio() -> f64 {
    0.5
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
    #[serde(default)]
    pub binary_path: String,
    /// 起動引数（ACP 等、コマンド + フラグでプロバイダを起こすもの向け）。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub sandbox: String,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default = "default_codex_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub auth_file: String,
    #[serde(default)]
    pub reasoning_effort: String,
    #[serde(default)]
    pub include_reasoning_encrypted_content: bool,
}

fn default_codex_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct FallbackConfig {
    #[serde(default)]
    pub chain: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
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
    /// DMに応答するオーナーのDiscord User ID（設定時、このID以外からのDMは無視）
    #[serde(default)]
    pub owner_discord_id: String,
    /// ハートビートメッセージを送信するDiscordチャンネルID
    #[serde(default)]
    pub heartbeat_channel_id: Option<u64>,
    /// spawn_subtask.webhook が省略された時に使うデフォルトの lifecycle webhook。
    #[serde(default)]
    pub default_subtask_webhook: Option<SubtaskWebhookConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SubtaskWebhookConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub events: Option<Vec<String>>,
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
pub(crate) fn expand_env_vars(input: &str) -> String {
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

// ---------- Provider overrides (dashboard-managed) ----------

/// TOML の LlmConfig に DB のプロバイダーオーバーライドを適用した実効設定を返す。
///
/// マージ規則:
/// - `enabled == Some(false)`: プロバイダーを実効設定から**除外**する
///   （TOML にキーがあっても登録されない）。
/// - `enabled == Some(true)` で TOML に無いプロバイダー: 空の ProviderConfig を
///   作ってオーバーライドを適用する（ollama 等のローカル系を UI から有効化する経路）。
/// - `api_key` / `base_url` / `default_model` は Some のフィールドだけ上書き。
///   Some("") は「TOML 値の消去」として扱う。
pub fn apply_llm_overrides(
    base: &LlmConfig,
    overrides: &[opencrab_db::queries::LlmProviderOverrideRow],
) -> LlmConfig {
    let mut cfg = base.clone();
    for row in overrides {
        if row.enabled == Some(false) {
            cfg.providers.remove(&row.provider);
            continue;
        }
        let entry = cfg.providers.entry(row.provider.clone()).or_default();
        if let Some(key) = &row.api_key {
            entry.api_key = key.clone();
        }
        if let Some(url) = &row.base_url {
            entry.base_url = url.clone();
        }
        if let Some(model) = &row.default_model {
            entry.default_model = model.clone();
        }
        if let Some(effort) = &row.reasoning_effort {
            entry.reasoning_effort = effort.clone();
        }
    }
    cfg
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
                    // GPT-5 系 / o シリーズを使うときの reasoning_effort（任意）。
                    if !pconfig.reasoning_effort.is_empty() {
                        p = p.with_reasoning_effort(&pconfig.reasoning_effort);
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
            "codex" => {
                let mut p = opencrab_llm::CodexProvider::new();
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_codex_path(&pconfig.binary_path);
                }
                if !pconfig.sandbox.is_empty() {
                    p = p.with_sandbox(&pconfig.sandbox);
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                // reasoning effort の上書き（gpt-5.6 系の既定 high を下げる等）。
                if !pconfig.reasoning_effort.is_empty() {
                    p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "cursor" => {
                let mut p = opencrab_llm::CursorProvider::new();
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_binary_path(&pconfig.binary_path);
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                // config に api_key があれば CURSOR_API_KEY として渡す。
                // 無ければ `cursor-agent login` 済みのアンビエント認証に任せる。
                if !pconfig.api_key.is_empty() {
                    p = p.with_api_key(&pconfig.api_key);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "acp" => {
                // ACP（Agent Client Protocol）エージェントを JSON-RPC/stdio で駆動する。
                // 起動コマンド/引数はエージェント毎に異なるため binary_path + args で指定。
                let mut p = opencrab_llm::AcpProvider::new();
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_binary_path(&pconfig.binary_path);
                }
                if !pconfig.args.is_empty() {
                    p = p.with_args(pconfig.args.clone());
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "chatgpt" => {
                let mut p = ChatGptProvider::new();
                if !pconfig.auth_file.is_empty() {
                    p = p.with_auth_file(&pconfig.auth_file);
                }
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.reasoning_effort.is_empty() {
                    p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                }
                p = p.with_include_encrypted_content(pconfig.include_reasoning_encrypted_content);
                Some(Arc::new(p))
            }
            "bonsai" => {
                let mut p = LlamaCppProvider::new().with_name("bonsai");
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
    let registered: Vec<String> = router
        .provider_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
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
    fn test_apply_llm_overrides() {
        use opencrab_db::queries::LlmProviderOverrideRow;
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: "toml-key".to_string(),
                base_url: "https://toml.example".to_string(),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key: "ant-key".to_string(),
                ..Default::default()
            },
        );
        let base = LlmConfig {
            providers,
            ..toml::from_str("").unwrap()
        };

        let overrides = vec![
            // openai: キーだけ DB 側で差し替え
            LlmProviderOverrideRow {
                provider: "openai".to_string(),
                api_key: Some("db-key".to_string()),
                ..Default::default()
            },
            // anthropic: 強制無効
            LlmProviderOverrideRow {
                provider: "anthropic".to_string(),
                enabled: Some(false),
                ..Default::default()
            },
            // ollama: TOML に無いが UI から有効化（base_url のみ）
            LlmProviderOverrideRow {
                provider: "ollama".to_string(),
                enabled: Some(true),
                base_url: Some("http://localhost:11434".to_string()),
                ..Default::default()
            },
        ];

        let merged = apply_llm_overrides(&base, &overrides);
        assert_eq!(merged.providers["openai"].api_key, "db-key");
        // 上書きしていないフィールドは TOML 値を維持
        assert_eq!(merged.providers["openai"].base_url, "https://toml.example");
        assert!(
            !merged.providers.contains_key("anthropic"),
            "disabled provider must be removed"
        );
        assert_eq!(
            merged.providers["ollama"].base_url,
            "http://localhost:11434"
        );
    }

    #[test]
    fn test_apply_llm_overrides_empty_is_identity() {
        let base: LlmConfig = toml::from_str("").unwrap();
        let merged = apply_llm_overrides(&base, &[]);
        assert_eq!(merged.providers.len(), base.providers.len());
        assert_eq!(merged.default_provider, base.default_provider);
    }

    #[test]
    fn test_voice_config_parses() {
        let toml_str = r#"
[voice]
enabled = true

[voice.stt]
provider = "openai"
language = "ja"

[voice.tts]
provider = "voicevox"
default_voice = "3"

[voice.tts.agent_voices]
crab = "3"
rabomi = "1"
"#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("voice config must parse");
        assert!(cfg.voice.enabled);
        assert_eq!(cfg.voice.stt.provider, "openai");
        assert_eq!(cfg.voice.stt.language.as_deref(), Some("ja"));
        assert_eq!(cfg.voice.tts.voice_for_agent("crab"), "3");
        assert_eq!(cfg.voice.tts.voice_for_agent("rabomi"), "1");
        assert_eq!(cfg.voice.tts.voice_for_agent("unknown"), "3");
    }

    #[test]
    fn test_voice_disabled_by_default() {
        let cfg: AppConfig = toml::from_str("").expect("empty config must parse");
        assert!(!cfg.voice.enabled);
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
