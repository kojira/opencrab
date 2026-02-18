use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// アクション実行結果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
    pub side_effects: Vec<SideEffect>,
}

impl ActionResult {
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            side_effects: vec![],
        }
    }

    pub fn error(msg: &str) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.to_string()),
            side_effects: vec![],
        }
    }

    pub fn with_side_effect(mut self, effect: SideEffect) -> Self {
        self.side_effects.push(effect);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SideEffect {
    MessageSent { channel: String, content: String },
    SkillAcquired { skill_id: String },
    FileWritten { path: String },
    LlmSwitched { purpose: String, model: String },
}

/// Discord/ゲートウェイ管理トレイト
///
/// サーバー一覧・チャンネル一覧の取得など、ゲートウェイ管理操作を抽象化する。
/// 実装はserverクレート側で行う（serenity依存を分離）。
#[async_trait]
pub trait GatewayAdmin: Send + Sync {
    /// Botが参加しているサーバー一覧を取得
    async fn list_guilds(&self) -> anyhow::Result<Vec<GuildInfo>>;
    /// 指定サーバーのチャンネル一覧を取得
    async fn list_channels(&self, guild_id: &str) -> anyhow::Result<Vec<ChannelInfo>>;
}

/// サーバー情報
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildInfo {
    pub id: String,
    pub name: String,
    pub member_count: Option<u64>,
}

/// チャンネル情報
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// アクション実行コンテキスト
pub struct ActionContext {
    pub agent_id: String,
    pub agent_name: String,
    pub session_id: Option<String>,
    pub db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    pub workspace: Arc<opencrab_core::workspace::Workspace>,
    /// Shared last metrics ID, updated by LlmRouterAdapter after each LLM call.
    /// Used by evaluate_response to auto-link evaluations.
    pub last_metrics_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Shared model override: when set by select_llm, SkillEngine uses this model.
    pub model_override: Arc<std::sync::Mutex<Option<String>>>,
    /// Shared current purpose: select_llm can set this to tag subsequent LLM calls.
    pub current_purpose: Arc<std::sync::Mutex<String>>,
    /// Runtime system information (model name, provider, etc.)
    pub runtime_info: Arc<std::sync::Mutex<RuntimeInfo>>,
    /// Gateway admin operations (Discord guild/channel management).
    /// None when running via REST API or in tests.
    pub gateway_admin: Option<Arc<dyn GatewayAdmin>>,
}

/// エージェントの実行環境情報
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub default_model: String,
    pub active_model: Option<String>,
    pub available_providers: Vec<String>,
    pub gateway: String,
}

/// アクション定義（Function Calling用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// アクション実行トレイト
#[async_trait]
pub trait Action: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    async fn execute(
        &self,
        args: &serde_json::Value,
        ctx: &ActionContext,
    ) -> ActionResult;
}
