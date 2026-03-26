//! AgentRunner trait implementation for AppState.
//!
//! Bridges the discord crate's AgentRunner trait to the server's
//! process module, breaking the circular dependency.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_gateway::GatewayActions;

use crate::process;
use crate::AppState;

#[async_trait]
impl opencrab_discord::AgentRunner for AppState {
    fn db(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.db
    }

    fn tools_config(&self) -> &Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>> {
        &self.tools_config
    }

    fn has_llm_providers(&self) -> bool {
        !self.llm_router.provider_names().is_empty()
    }

    fn build_agent_context(&self, agent_id: &str) -> (String, String) {
        let conn = self.db.lock().unwrap();
        process::build_agent_context(&conn, agent_id)
    }

    fn build_conversation_string(
        &self,
        session_id: &str,
        agent_id: &str,
        context_budget_tokens: usize,
    ) -> Result<String, anyhow::Error> {
        let conn = self.db.lock().unwrap();
        process::build_conversation_string(&conn, session_id, agent_id, context_budget_tokens)
    }

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
    ) -> anyhow::Result<opencrab_core::EngineResult> {
        process::run_agent_response(
            self,
            agent_id,
            agent_name,
            session_id,
            system_prompt,
            conversation,
            gateway_name,
            gateway_actions,
            caller,
            image_urls,
            depth,
            trigger_message_id,
            on_response_text,
        )
        .await
    }

    fn create_llm_client(&self) -> Arc<dyn opencrab_core::LlmClient> {
        Arc::new(crate::llm_adapter::LlmRouterAdapter::new(
            self.llm_router.clone(),
        ))
    }

    fn default_model(&self) -> String {
        self.default_model.clone()
    }

    fn context_budget_tokens(&self) -> usize {
        let conn = self.db.lock().unwrap();
        let parts: Vec<&str> = self.default_model.splitn(2, ':').collect();
        if parts.len() == 2 {
            process::compute_context_budget(&conn, parts[0], parts[1], self.compaction_ratio)
        } else {
            100_000 // fallback
        }
    }

    fn workspace_base(&self) -> &str {
        &self.workspace_base
    }
}
