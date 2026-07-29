//! NostrAgentRunner trait implementation for AppState.
//!
//! nostr ゲートウェイ（crates/nostr）の最小 runner を、既存の process /
//! transcript ヘルパへ委譲して実装する（discord の AgentRunner impl と同型）。
//!
//! ゲートウェイ非依存なメソッドは `agent_runtime_impl.rs` の
//! [`opencrab_actions::AgentRuntime`] 実装が持つ（#156 S1）。転記（受信イベント /
//! エージェント返信）も同様にそちらへ移した（#158 S3）。

use crate::AppState;

impl opencrab_nostr::NostrAgentRunner for AppState {
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

    fn upsert_nostr_config(
        &self,
        cfg: &opencrab_db::queries::AgentNostrConfigRow,
    ) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(&conn, cfg)?;
        Ok(())
    }

    fn set_nostr_enabled(&self, agent_id: &str, enabled: bool) -> anyhow::Result<()> {
        let conn = self.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_enabled(&conn, agent_id, enabled)?;
        Ok(())
    }

    /// エージェント宛の Nostr 受信を転記する宛先を解決する（issue #252 段階 A）。
    ///
    /// 同期 DB 読み 1 回。fail-closed（未設定 / 無効 / 不正 → `None`）の判定は actions 層の
    /// `resolve_nostr_relay_webhook` に集約してあるので、ここは委譲するだけ。
    fn resolve_nostr_relay_target(
        &self,
        agent_id: &str,
    ) -> Option<opencrab_actions::webhook_target::WebhookConfig> {
        let conn = self.db.lock().unwrap();
        opencrab_actions::webhook_target::resolve_nostr_relay_webhook(&conn, agent_id)
    }

    /// 解決済みの宛先へ転記本文を**非ブロック**で送る（issue #252 段階 A）。
    ///
    /// Discord の content 上限（2000 文字）に合わせて分割し、1 つの spawn タスクで順に送る
    /// （fire-and-forget で受信ループを止めない）。送信失敗は**ログのみ**で、応答生成や他
    /// セッションの受信を巻き込まない。生 URL はログに出さない。
    fn relay_inbound_notification(
        &self,
        target: &opencrab_actions::webhook_target::WebhookConfig,
        text: String,
    ) {
        const DISCORD_CONTENT_LIMIT: usize = 2000;
        let url = target.url.clone();
        let chunks = opencrab_actions::webhook_target::chunk_text(&text, DISCORD_CONTENT_LIMIT);
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            for chunk in chunks {
                // allowed_mentions を必ず抑止して送る（mention 暴発対策）。
                // 詳細は webhook_target::build_relay_webhook_body の doc を参照。
                let body = opencrab_actions::webhook_target::build_relay_webhook_body(&chunk);
                match client.post(&url).json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => {}
                    Ok(resp) => {
                        tracing::warn!(
                            status = resp.status().as_u16(),
                            "Nostr 受信の Discord 転記が非成功ステータスで失敗（ログのみ）"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Nostr 受信の Discord 転記の送信に失敗（ログのみ）"
                        );
                    }
                }
            }
        });
    }
}
