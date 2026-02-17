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

/// アクション実行コンテキスト
pub struct ActionContext {
    pub agent_id: String,
    pub agent_name: String,
    pub session_id: Option<String>,
    pub db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    pub workspace: Arc<opencrab_core::workspace::Workspace>,
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
