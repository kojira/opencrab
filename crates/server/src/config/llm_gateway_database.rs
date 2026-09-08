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
    /// 高水位比: `input_high = min(floor(W * この比), A)`。既定 0.50。
    #[serde(default = "default_compaction_ratio")]
    pub compaction_ratio: f64,
    /// 低水位比: `input_low = min(floor(W * この比), floor(A / 2))`。既定 0.25。
    #[serde(default = "default_input_low_ratio")]
    pub input_low_ratio: f64,
    /// 絶対上限 A（token）。較正前は 80_000（85–90K 劣化開始点より安全側）。
    #[serde(default = "default_absolute_input_cap")]
    pub absolute_input_cap: usize,
    /// Memory Index の個別上限（token）。会話の余りを暗黙に流用しない。
    #[serde(default = "default_memory_index_token_cap")]
    pub memory_index_token_cap: usize,
    /// functions の個別上限（token）。縮約せず、超過は `context_budget_exhausted`。
    #[serde(default = "default_functions_token_cap")]
    pub functions_token_cap: usize,
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
            input_low_ratio: default_input_low_ratio(),
            absolute_input_cap: default_absolute_input_cap(),
            memory_index_token_cap: default_memory_index_token_cap(),
            functions_token_cap: default_functions_token_cap(),
        }
    }
}

fn default_compaction_ratio() -> f64 {
    opencrab_core::context_budget::DEFAULT_INPUT_HIGH_RATIO
}

fn default_input_low_ratio() -> f64 {
    opencrab_core::context_budget::DEFAULT_INPUT_LOW_RATIO
}

fn default_absolute_input_cap() -> usize {
    opencrab_core::context_budget::DEFAULT_ABSOLUTE_CAP_A
}

fn default_memory_index_token_cap() -> usize {
    opencrab_core::context_budget::DEFAULT_MEMORY_INDEX_TOKEN_CAP
}

fn default_functions_token_cap() -> usize {
    opencrab_core::context_budget::DEFAULT_FUNCTIONS_TOKEN_CAP
}

fn default_provider() -> String {
    "openai".to_string()
}
fn default_model() -> String {
    "gpt-4o".to_string()
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ProviderConfig {
    /// API 形式（どのクライアント実装で喋るか）。セクションキーが「名乗り名」
    /// （接続先の実体）であるのに対し、これは「形式」を表す。省略時はセクション
    /// キーと同値とみなす（`build_llm_router` の解決規則）。同じ形式の接続先を
    /// 別名で 2 つ以上持てるようにするための分離。
    #[serde(default, rename = "type")]
    pub provider_type: String,
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

