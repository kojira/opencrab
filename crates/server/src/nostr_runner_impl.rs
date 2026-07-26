//! NostrAgentRunner trait implementation for AppState.
//!
//! nostr ゲートウェイ（crates/nostr）の最小 runner を、既存の process /
//! transcript ヘルパへ委譲して実装する（discord の AgentRunner impl と同型）。
//!
//! ゲートウェイ非依存なメソッドは `agent_runtime_impl.rs` の
//! [`opencrab_actions::AgentRuntime`] 実装が持つ（#156 S1）。

use crate::AppState;

impl opencrab_nostr::NostrAgentRunner for AppState {
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

    fn get_nostr_config(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_db::queries::AgentNostrConfigRow> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, agent_id).unwrap_or(None)
    }

    fn set_nostr_secret_key(&self, agent_id: &str, secret_key: &str) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_secret_key(&conn, agent_id, secret_key)?;
        Ok(())
    }
}
