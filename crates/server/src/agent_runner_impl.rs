//! AgentRunner trait implementation for AppState.
//!
//! Bridges the discord crate's AgentRunner trait to the server's
//! process module, breaking the circular dependency.

use std::sync::Arc;

use async_trait::async_trait;

use crate::process;
use crate::AppState;

#[async_trait]
impl opencrab_discord::AgentRunner for AppState {
    fn db(&self) -> &opencrab_db::Db {
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
        req: opencrab_actions::RunRequest,
    ) -> anyhow::Result<opencrab_core::EngineResult> {
        process::run_agent_response(self, req).await
    }

    fn create_llm_client(&self) -> Arc<dyn opencrab_core::LlmClient> {
        Arc::new(crate::llm_adapter::LlmRouterAdapter::new(
            self.llm_router.clone(),
        ))
    }

    fn default_model(&self) -> String {
        self.default_model.clone()
    }

    fn context_budget_tokens(&self, agent_id: &str) -> usize {
        let conn = self.db.lock().unwrap();
        let eff =
            opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &self.default_model)
                .unwrap_or_else(|_| self.default_model.clone());
        let (prov, mdl) = process::split_llm_model_spec(&eff);
        process::compute_context_budget(&conn, prov, mdl, self.compaction_ratio)
    }

    fn workspace_base(&self) -> &str {
        &self.workspace_base
    }
}
