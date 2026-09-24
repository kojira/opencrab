use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Nostr Relay Config（Nostr 受信を Discord へ転記する宛先 / issue #252 段階 A）
// ============================================
//
// エージェントが Nostr で受け取った**自分宛の受信**（メンション/リプライ/DM）を、
// エージェント単位で設定した 1 つの転記先（webhook）へ流すための設定。
//
// - `enabled`: 既定 **0（無効）**。行を作っただけでは転記しない（fail-closed）。
// - `webhook_url`: 転記先の webhook URL。NULL / 空なら転記しない。URL の妥当性検証は
//   ここ（db クレート）では行わない — 検証と actions 型（`WebhookConfig`）への変換は
//   上位（`opencrab_actions::webhook_target::resolve_nostr_relay_webhook`）が担う。
//   db クレートは Discord/webhook の語彙に依存せず、生の行だけを扱う。

/// エージェント単位の Nostr 受信転記先 1 行（`agent_nostr_relay_config`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNostrRelayConfigRow {
    pub agent_id: String,
    pub enabled: bool,
    /// 転記先 webhook URL。`None` = 未設定（転記しない）。
    pub webhook_url: Option<String>,
}

/// 設定行を取得する。行が無ければ `None`。
pub fn get_agent_nostr_relay_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<AgentNostrRelayConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, enabled, webhook_url FROM agent_nostr_relay_config WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentNostrRelayConfigRow {
                agent_id: row.get(0)?,
                enabled: row.get(1)?,
                webhook_url: row.get(2)?,
            })
        },
    );
    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 設定行を作成/更新する（既存行は agent_id 主キーで上書き）。
pub fn upsert_agent_nostr_relay_config(
    conn: &Connection,
    cfg: &AgentNostrRelayConfigRow,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_nostr_relay_config (agent_id, enabled, webhook_url, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
            enabled = excluded.enabled,
            webhook_url = excluded.webhook_url,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.enabled,
            cfg.webhook_url,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        crate::init_memory().unwrap()
    }

    #[test]
    fn test_missing_row_is_none() {
        let conn = mem();
        assert_eq!(
            get_agent_nostr_relay_config(&conn, "a1").unwrap(),
            None,
            "設定が無いエージェントは None（呼び出し側で fail-closed）"
        );
    }

    #[test]
    fn test_upsert_creates_then_updates_single_row() {
        let conn = mem();
        upsert_agent_nostr_relay_config(
            &conn,
            &AgentNostrRelayConfigRow {
                agent_id: "a1".to_string(),
                enabled: false,
                webhook_url: Some("https://discord.com/api/webhooks/1/tok".to_string()),
            },
        )
        .unwrap();
        let got = get_agent_nostr_relay_config(&conn, "a1").unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(
            got.webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/1/tok")
        );

        // 上書き（有効化 + URL 変更）。
        upsert_agent_nostr_relay_config(
            &conn,
            &AgentNostrRelayConfigRow {
                agent_id: "a1".to_string(),
                enabled: true,
                webhook_url: Some("https://discord.com/api/webhooks/2/tok2".to_string()),
            },
        )
        .unwrap();
        let got = get_agent_nostr_relay_config(&conn, "a1").unwrap().unwrap();
        assert!(got.enabled);
        assert_eq!(
            got.webhook_url.as_deref(),
            Some("https://discord.com/api/webhooks/2/tok2")
        );

        // 主キー衝突で行は 1 件のまま。
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_nostr_relay_config", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn test_config_is_per_agent() {
        let conn = mem();
        upsert_agent_nostr_relay_config(
            &conn,
            &AgentNostrRelayConfigRow {
                agent_id: "a1".to_string(),
                enabled: true,
                webhook_url: Some("https://discord.com/api/webhooks/1/tok".to_string()),
            },
        )
        .unwrap();
        // 別エージェントの設定は独立（行が無い）。
        assert_eq!(get_agent_nostr_relay_config(&conn, "a2").unwrap(), None);
    }
}
