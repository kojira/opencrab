//! [`AgentRuntime`] の `AppState` 実装（#156 S1）。
//!
//! 以前は同じ本体が `agent_runner_impl.rs`（discord）/ `nostr_runner_impl.rs` /
//! `web_runner_impl.rs` の 3 箇所にコピーされていた。ゲートウェイ非依存な実行・
//! セッション管理はここ 1 箇所だけが持ち、各ゲートウェイの impl はそれぞれの語彙を
//! 含むメソッドだけを実装する。
//!
//! discord feature の有無に関わらずコンパイルされる（nostr / web も同じ実装を使う）。

use async_trait::async_trait;

use opencrab_actions::AgentRuntime;

use crate::process;
use crate::AppState;

#[async_trait]
impl AgentRuntime for AppState {
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

    fn has_llm_providers(&self) -> bool {
        !self.llm_router.get().provider_names().is_empty()
    }

    fn record_agent_no_reply(&self, agent_id: &str, session_id: &str) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_agent_no_reply(&conn, agent_id, session_id);
        }
    }

    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
        mode: &str,
    ) {
        let Ok(conn) = self.db.lock() else { return };
        if let Ok(Some(existing)) = opencrab_db::queries::get_session(&conn, session_id) {
            // 既存セッションに metadata が無ければ補完する（後方互換）。mode は触らない。
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
            mode: mode.to_string(),
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

    fn session_theme(&self, session_id: &str) -> Option<String> {
        self.db
            .lock()
            .ok()
            .and_then(|conn| {
                opencrab_db::queries::get_session(&conn, session_id)
                    .ok()
                    .flatten()
            })
            .map(|s| s.theme)
    }

    fn mark_interaction_status(
        &self,
        interaction_id: &str,
        status: &str,
        response_json: Option<&str>,
        responder_id: Option<&str>,
    ) {
        if let Ok(conn) = self.db.lock() {
            opencrab_db::queries::update_pending_interaction_status(
                &conn,
                interaction_id,
                status,
                response_json,
                responder_id,
            )
            .ok();
        }
    }

    fn cleanup_stale_interactions(&self) {
        let Ok(conn) = self.db.lock() else { return };
        if let Ok(count) = opencrab_db::queries::cleanup_stale_pending_interactions(&conn) {
            if count > 0 {
                tracing::info!(count = count, "Cleaned up stale pending interactions");
            }
        }
    }
}
