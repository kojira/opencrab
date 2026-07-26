//! `WebAgentRunner` trait implementation for AppState.
//!
//! web ゲートウェイ（`crates/web-gateway`）の最小 runner を、既存の process /
//! transcript / queries ヘルパへ委譲して実装する（nostr の `NostrAgentRunner`
//! impl と同型）。DB 行の型はここで閉じ、ゲートウェイ側へは出さない。
//!
//! ゲートウェイ非依存なメソッドは `agent_runtime_impl.rs` の
//! [`opencrab_actions::AgentRuntime`] 実装が持つ（#156 S1）。

use std::sync::Arc;

use opencrab_web_gateway::{WebAgentRunner, WebGateway, WEB_SESSION_THEME};

use crate::AppState;

impl WebAgentRunner for AppState {
    /// 呼び出し元の権限判定。既存 REST（`agents_messages`）に倣い trusted_users から
    /// caller を導出し、未登録なら Discord 設定の owner と突き合わせる（#164）。
    ///
    /// 引く経路は `web`（#214）。ただし当面は互換読み
    /// （`get_trusted_user_with_legacy_fallback` — 自経路の行が無ければ従来の `discord`
    /// 経路も見る）を通す。理由と外す条件はその関数の doc を参照。
    ///
    /// なお owner 判定に使う設定はここでも Discord の owner のまま（web 専用の owner を
    /// 新設しない）。認可の判定をこの経路に新設しないための線引き。
    fn resolve_caller(&self, agent_id: &str, user_id: &str) -> opencrab_actions::CallerIdentity {
        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::get_trusted_user_with_legacy_fallback(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            user_id,
            agent_id,
        ) {
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

    fn ensure_web_session(&self, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
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
        match self.db.lock() {
            Ok(conn) => {
                crate::transcript::record_rest_agent_reply(
                    &conn,
                    agent_id,
                    session_id,
                    text,
                    iterations,
                    tool_calls_made,
                );
            }
            Err(_) => {
                // best-effort な転記だが、黙って落とすと「SSE には返信が流れたのに
                // 履歴に無い」状態が痕跡なしで生まれる（次ターンで言い直す）。
                tracing::error!(
                    agent_id = %agent_id,
                    session_id = %session_id,
                    "web: DB ロックが毒化しており応答を転記できなかった"
                );
            }
        }
    }

    fn web_gateway(&self) -> &Arc<WebGateway> {
        &self.web_gateway
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::CallerIdentity;

    fn register(state: &AppState, platform: &str, user_id: &str, permission: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_trusted_user(
            &conn,
            platform,
            &format!("row-{platform}-{user_id}"),
            "agent-1",
            user_id,
            permission,
            "owner",
            "2026-01-01",
            "",
        )
        .unwrap();
    }

    /// #214 の互換読み: web 経路の行がまだ無い環境では、従来（`discord`）経路の行で
    /// 今までどおり信頼が効く。ここが落ちると既存の信頼済みユーザーが一斉に権限を失う。
    #[test]
    fn resolve_caller_falls_back_to_legacy_discord_rows() {
        let state = crate::test_app_state();
        register(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "42",
            "user",
        );
        assert_eq!(
            state.resolve_caller("agent-1", "42"),
            CallerIdentity::TrustedUser
        );
    }

    /// 自経路（web）の行があればそれで判定する。
    #[test]
    fn resolve_caller_uses_web_platform_row_when_present() {
        let state = crate::test_app_state();
        register(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_WEB,
            "dash-user",
            "co_agent",
        );
        assert_eq!(
            state.resolve_caller("agent-1", "dash-user"),
            CallerIdentity::CoAgent {
                agent_id: "dash-user".to_string()
            }
        );
    }

    /// 未登録の識別子は最小権限のまま（互換読みが「誰でも通る」に化けていない）。
    #[test]
    fn resolve_caller_denies_unknown_user() {
        let state = crate::test_app_state();
        register(
            &state,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            "42",
            "user",
        );
        assert_eq!(
            state.resolve_caller("agent-1", "999"),
            CallerIdentity::Agent
        );
    }
}
