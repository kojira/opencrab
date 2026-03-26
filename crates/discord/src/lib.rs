//! Discord integration crate for OpenCrab.
//!
//! Provides Discord gateway actions, message processing loop, and per-agent bot management.
//! All Discord-specific logic lives here, keeping the server crate Discord-free.

pub mod gateway_actions;
pub mod manager;
pub mod message_loop;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_gateway::GatewayActions;

pub use gateway_actions::DiscordGatewayActions;
pub use gateway_actions::{
    CompletionRegistry, SpawnedSubtask, SubtaskCompletionFn, SubtaskRegistry,
};
pub use manager::DiscordGatewayManager;
pub use message_loop::run_discord_loop;

/// Trait abstracting the server-side agent processing pipeline.
///
/// Defined here (in the discord crate) to break the circular dependency:
/// discord needs to invoke agent processing, but server depends on discord.
/// Server implements this trait for its `AppState`.
#[async_trait]
pub trait AgentRunner: Send + Sync + Clone + 'static {
    /// Access the shared database connection.
    fn db(&self) -> &Arc<Mutex<rusqlite::Connection>>;

    /// Access the shared tools configuration.
    fn tools_config(&self) -> &Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>;

    /// Whether any LLM providers are configured.
    fn has_llm_providers(&self) -> bool;

    /// Build the agent's system prompt and name from DB.
    ///
    /// Returns `(system_prompt, agent_name)`.
    fn build_agent_context(&self, agent_id: &str) -> (String, String);

    /// Build the conversation history string for a session (with compaction).
    fn build_conversation_string(
        &self,
        session_id: &str,
        agent_id: &str,
        context_budget_tokens: usize,
    ) -> Result<String, anyhow::Error>;

    /// Run the full agent response pipeline (SkillEngine + LLM).
    async fn run_agent_response(
        &self,
        agent_id: &str,
        agent_name: &str,
        session_id: &str,
        system_prompt: &str,
        conversation: &str,
        gateway_name: &str,
        gateway_actions: Option<Arc<dyn GatewayActions>>,
        caller: opencrab_actions::CallerIdentity,
        image_urls: &[String],
        depth: u32,
        trigger_message_id: Option<String>,
        on_response_text: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    ) -> anyhow::Result<opencrab_core::EngineResult>;

    /// エージェントのLLMクライアントを生成する。
    fn create_llm_client(&self) -> Arc<dyn opencrab_core::LlmClient>;

    /// デフォルトモデル名を返す（"provider:model" 形式）。
    fn default_model(&self) -> String;

    /// 会話コンテキストのトークン予算を返す（context_window * compaction_ratio）。
    fn context_budget_tokens(&self) -> usize;

    /// ワークスペースベースパスを返す（例: "/data/workspace/{agent_id}"）。
    fn workspace_base(&self) -> &str;
}
