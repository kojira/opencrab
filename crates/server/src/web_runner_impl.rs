//! `WebAgentRunner` trait implementation for AppState.
//!
//! web ゲートウェイ（`crates/web-gateway`）の最小 runner を、既存の process /
//! transcript / queries ヘルパへ委譲して実装する（nostr の `NostrAgentRunner`
//! impl と同型）。DB 行の型はここで閉じ、ゲートウェイ側へは出さない。

use std::sync::Arc;

use async_trait::async_trait;

use opencrab_web_gateway::{WebAgentRunner, WebGateway, WEB_SESSION_THEME};

use crate::process;
use crate::AppState;

#[async_trait]
impl WebAgentRunner for AppState {
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

    fn has_llm_provider(&self) -> bool {
        !self.llm_router.get().provider_names().is_empty()
    }

    /// 呼び出し元の権限判定。既存 REST（`agents_messages`）に倣い trusted_users から
    /// caller を導出し、未登録なら Discord 設定の owner と突き合わせる（#164）。
    fn resolve_caller(&self, agent_id: &str, user_id: &str) -> opencrab_actions::CallerIdentity {
        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::get_trusted_user(&conn, user_id, agent_id) {
            Some(u) if u.permission == "co_agent" => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: user_id.to_string(),
            },
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
            None => {
                let cfg = opencrab_db::queries::get_agent_discord_config(&conn, agent_id);
                if let Ok(Some(c)) = cfg {
                    if crate::api::is_owner_id(&c.owner_discord_id, user_id) {
                        opencrab_actions::CallerIdentity::Owner
                    } else {
                        opencrab_actions::CallerIdentity::Agent
                    }
                } else {
                    opencrab_actions::CallerIdentity::Agent
                }
            }
        }
    }

    fn ensure_session(&self, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        let existing = opencrab_db::queries::get_session(&conn, session_id)
            .ok()
            .flatten();
        if existing.is_some() {
            return Ok(());
        }
        let session = opencrab_db::queries::SessionRow {
            id: session_id.to_string(),
            mode: "autonomous".to_string(),
            theme: WEB_SESSION_THEME.to_string(),
            phase: "divergent".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: serde_json::json!([agent_id]).to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        };
        opencrab_db::queries::insert_session(&conn, &session)?;
        Ok(())
    }

    fn record_user_message(
        &self,
        agent_id: &str,
        session_id: &str,
        user_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(user_id.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&conn, &log)?;
        Ok(())
    }

    fn record_agent_reply(
        &self,
        agent_id: &str,
        session_id: &str,
        text: &str,
        iterations: usize,
        tool_calls_made: usize,
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_rest_agent_reply(
                &conn,
                agent_id,
                session_id,
                text,
                iterations,
                tool_calls_made,
            );
        }
    }

    fn web_gateway(&self) -> &Arc<WebGateway> {
        &self.web_gateway
    }
}
