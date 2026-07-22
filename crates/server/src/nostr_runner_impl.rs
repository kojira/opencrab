//! NostrAgentRunner trait implementation for AppState.
//!
//! nostr ゲートウェイ（crates/nostr）の最小 runner を、既存の process /
//! transcript ヘルパへ委譲して実装する（discord の AgentRunner impl と同型）。

use async_trait::async_trait;

use crate::process;
use crate::AppState;

#[async_trait]
impl opencrab_nostr::NostrAgentRunner for AppState {
    async fn run_agent_response(
        &self,
        req: opencrab_actions::RunRequest,
    ) -> anyhow::Result<opencrab_core::EngineResult> {
        process::run_agent_response(self, req).await
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
    ) -> anyhow::Result<String> {
        let conn = self.db.lock().unwrap();
        process::build_conversation_string(&conn, session_id, agent_id, context_budget_tokens)
    }

    fn context_budget_tokens(&self, agent_id: &str) -> usize {
        let conn = self.db.lock().unwrap();
        let eff =
            opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &self.default_model)
                .unwrap_or_else(|_| self.default_model.clone());
        let (prov, mdl) = process::split_llm_model_spec(&eff);
        process::compute_context_budget(&conn, prov, mdl, self.compaction_ratio)
    }

    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
    ) {
        // discord の ensure_session と同じ形（mode は "nostr"）。discord feature に
        // 依存しないよう DB 操作を直接行う。
        let Ok(conn) = self.db.lock() else { return };
        if let Ok(Some(existing)) = opencrab_db::queries::get_session(&conn, session_id) {
            if existing.metadata_json.is_none() {
                opencrab_db::queries::update_session_metadata(
                    &conn,
                    session_id,
                    metadata_json,
                    theme,
                )
                .ok();
            }
            return;
        }
        let session = opencrab_db::queries::SessionRow {
            id: session_id.to_string(),
            mode: "nostr".to_string(),
            theme: theme.to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: serde_json::to_string(agent_ids).unwrap_or_default(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: Some(metadata_json.to_string()),
        };
        opencrab_db::queries::insert_session(&conn, &session).ok();
    }

    fn record_nostr_user_message(
        &self,
        session_id: &str,
        sender_pubkey: &str,
        sender_name: &str,
        text: &str,
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_nostr_user_message(
                &conn,
                session_id,
                sender_pubkey,
                sender_name,
                text,
            );
        }
    }

    fn record_nostr_agent_reply(&self, agent_id: &str, session_id: &str, text: &str) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_nostr_agent_reply(&conn, agent_id, session_id, text);
        }
    }

    fn list_enabled_nostr_configs(&self) -> Vec<opencrab_db::queries::AgentNostrConfigRow> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::list_enabled_agent_nostr_configs(&conn).unwrap_or_default()
    }
}
