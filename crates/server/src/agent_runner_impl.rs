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

    // ---- 転記（#42: 行の形は transcript モジュールが所有） ----

    fn record_user_message(
        &self,
        session_id: &str,
        sender_id: &str,
        sender_name: &str,
        avatar_url: Option<&str>,
        channel_id: &str,
        text: &str,
        image_urls: &[String],
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_discord_user_message(
                &conn,
                session_id,
                sender_id,
                sender_name,
                avatar_url,
                channel_id,
                text,
                image_urls,
            );
        }
    }

    fn record_agent_no_reply(&self, agent_id: &str, session_id: &str) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_agent_no_reply(&conn, agent_id, session_id);
        }
    }

    fn record_agent_reply(
        &self,
        agent_id: &str,
        session_id: &str,
        channel_id: &str,
        text: &str,
        context: opencrab_discord::DiscordReplyContext<'_>,
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_discord_agent_reply(
                &conn, agent_id, session_id, channel_id, text, &context,
            );
        }
    }

    fn record_interaction_response(
        &self,
        agent_id: &str,
        session_id: &str,
        record: opencrab_discord::InteractionRecord<'_>,
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_interaction_response(&conn, agent_id, session_id, &record);
        }
    }

    // ---- 判定（#43） ----

    fn is_channel_writable(&self, channel_id: &str) -> bool {
        self.db
            .lock()
            .map(|conn| opencrab_db::queries::is_channel_writable(&conn, channel_id))
            .unwrap_or(false)
    }

    fn is_channel_whitelisted_for_agent(&self, channel_id: &str, agent_id: &str) -> bool {
        self.db
            .lock()
            .map(|conn| {
                opencrab_db::queries::is_channel_whitelisted_for_agent(&conn, channel_id, agent_id)
            })
            .unwrap_or(false)
    }

    fn dm_allowed_any(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        owner_discord_id: &str,
    ) -> bool {
        if !owner_discord_id.is_empty() && sender_id == owner_discord_id {
            return true;
        }
        match self.db.lock() {
            Ok(conn) => {
                let any_trusted = agent_ids
                    .iter()
                    .any(|aid| opencrab_db::queries::is_trusted_user(&conn, sender_id, aid));
                let any_registered = agent_ids
                    .iter()
                    .any(|aid| opencrab_db::queries::trusted_user_count(&conn, aid) > 0);
                if any_registered {
                    any_trusted
                } else {
                    // 信頼ユーザー登録が全く無い場合は owner のみ許可（owner 未設定なら全許可）。
                    owner_discord_id.is_empty() || sender_id == owner_discord_id
                }
            }
            // DB接続取得失敗時は fail-closed。
            Err(_) => false,
        }
    }

    fn dm_allowed(&self, sender_id: &str, agent_id: &str, owner_discord_id: &str) -> bool {
        if !owner_discord_id.is_empty() && sender_id == owner_discord_id {
            return true;
        }
        match self.db.lock() {
            Ok(conn) => {
                if opencrab_db::queries::trusted_user_count(&conn, agent_id) > 0 {
                    opencrab_db::queries::is_trusted_user(&conn, sender_id, agent_id)
                } else {
                    // このエージェントに信頼ユーザー登録が無い場合は owner のみ許可。
                    owner_discord_id.is_empty() || sender_id == owner_discord_id
                }
            }
            Err(_) => false,
        }
    }

    fn resolve_caller(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        owner_discord_id: &str,
    ) -> opencrab_actions::CallerIdentity {
        if !owner_discord_id.is_empty() && sender_id == owner_discord_id {
            return opencrab_actions::CallerIdentity::Owner;
        }
        // DB接続取得失敗時は trust_info=None（＝最小権限の Agent 扱い）。
        let trust_info = match self.db.lock() {
            Ok(conn) => agent_ids
                .iter()
                .find_map(|aid| opencrab_db::queries::get_trusted_user(&conn, sender_id, aid)),
            Err(_) => None,
        };
        match trust_info {
            Some(u) if u.permission == "co_agent" => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: sender_id.to_string(),
            },
            Some(u) if u.permission == "owner" => opencrab_actions::CallerIdentity::Owner,
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
            None => opencrab_actions::CallerIdentity::Agent,
        }
    }

    // ---- セッション/インタラクション管理（#43） ----

    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
    ) {
        let Ok(conn) = self.db.lock() else { return };
        if let Ok(Some(existing)) = opencrab_db::queries::get_session(&conn, session_id) {
            // 既存セッションに metadata が無ければ補完する（後方互換）。
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
            mode: "discord".to_string(),
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

    fn list_enabled_discord_configs(&self) -> Vec<opencrab_db::queries::AgentDiscordConfigRow> {
        match self
            .db
            .lock()
            .map_err(anyhow::Error::from)
            .and_then(|conn| {
                opencrab_db::queries::list_enabled_agent_discord_configs(&conn)
                    .map_err(anyhow::Error::from)
            }) {
            Ok(configs) => configs,
            Err(e) => {
                // 起動時の復元経路で使われるため、失敗を黙って空にしない。
                tracing::warn!(error = %e, "Failed to load agent discord configs from DB");
                Vec::new()
            }
        }
    }

    fn served_by_dedicated_gateway(&self, agent_id: &str) -> bool {
        // DB の enabled フラグではなく manager の liveness で判定する（#40）。
        // enabled=1 でもゲートウェイが起動失敗/停止していれば false → 共有側が
        // フォールバックとして処理を続け、「誰も応答しない」状態を作らない。
        self.discord_manager
            .as_ref()
            .map(|m| m.is_running(agent_id))
            .unwrap_or(false)
    }
}
