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

    fn has_llm_providers(&self) -> bool {
        !self.llm_router.provider_names().is_empty()
    }

    fn build_agent_context(&self, agent_id: &str, theme: &str) -> (String, String) {
        let conn = self.db.lock().unwrap();
        process::build_agent_context(&conn, agent_id, theme)
    }

    fn build_conversation_string(&self, session_id: &str) -> String {
        let conn = self.db.lock().unwrap();
        process::build_conversation_string(&conn, session_id)
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
        )
        .await
    }
}
