use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Nostr Config（per-agent の Nostr sub-gateway 設定）
// ============================================
//
// 秘密鍵はエージェント毎に隔離する（鍵の共有事故を防ぐ）。relays / filter は
// JSON TEXT で保持し、server 層で NostrConfig にパースする（db クレートは
// opencrab-nostr に依存しない）。

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNostrConfigRow {
    pub agent_id: String,
    /// nsec1...（このエージェント固有の秘密鍵）。
    pub secret_key: String,
    /// 購読リレーの JSON 配列（例 `["wss://yabu.me"]`）。空配列なら既定を使う。
    pub relays_json: String,
    /// フィルタの JSON（`{"authors":[],"keywords":[],"kinds":[]}`）。
    pub filter_json: String,
    pub enabled: bool,
}

pub fn upsert_agent_nostr_config(conn: &Connection, cfg: &AgentNostrConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_nostr_config (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id) DO UPDATE SET
            secret_key = excluded.secret_key,
            relays_json = excluded.relays_json,
            filter_json = excluded.filter_json,
            enabled = excluded.enabled,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.secret_key,
            cfg.relays_json,
            cfg.filter_json,
            cfg.enabled,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_agent_nostr_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<AgentNostrConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, secret_key, relays_json, filter_json, enabled
         FROM agent_nostr_config WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentNostrConfigRow {
                agent_id: row.get(0)?,
                secret_key: row.get(1)?,
                relays_json: row.get(2)?,
                filter_json: row.get(3)?,
                enabled: row.get(4)?,
            })
        },
    );
    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_agent_nostr_config(conn: &Connection, agent_id: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM agent_nostr_config WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(deleted > 0)
}

pub fn set_agent_nostr_config_enabled(
    conn: &Connection,
    agent_id: &str,
    enabled: bool,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_nostr_config SET enabled = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![enabled, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

pub fn list_enabled_agent_nostr_configs(conn: &Connection) -> Result<Vec<AgentNostrConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, secret_key, relays_json, filter_json, enabled
         FROM agent_nostr_config WHERE enabled = 1",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AgentNostrConfigRow {
            agent_id: row.get(0)?,
            secret_key: row.get(1)?,
            relays_json: row.get(2)?,
            filter_json: row.get(3)?,
            enabled: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        crate::init_memory().unwrap()
    }

    #[test]
    fn test_upsert_get_roundtrip_and_enabled_filter() {
        let conn = mem();
        let cfg = AgentNostrConfigRow {
            agent_id: "agent-1".to_string(),
            secret_key: "nsec1abc".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: false,
        };
        upsert_agent_nostr_config(&conn, &cfg).unwrap();

        let got = get_agent_nostr_config(&conn, "agent-1").unwrap().unwrap();
        assert_eq!(got.secret_key, "nsec1abc");
        assert_eq!(got.relays_json, r#"["wss://yabu.me"]"#);
        assert!(!got.enabled);

        // disabled は列挙されない。
        assert!(list_enabled_agent_nostr_configs(&conn).unwrap().is_empty());

        // enable → 列挙される。
        assert!(set_agent_nostr_config_enabled(&conn, "agent-1", true).unwrap());
        let enabled = list_enabled_agent_nostr_configs(&conn).unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].agent_id, "agent-1");

        // 秘密鍵はエージェント毎に別行（共有されない）。
        let cfg2 = AgentNostrConfigRow {
            agent_id: "agent-2".to_string(),
            secret_key: "nsec1def".to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{}".to_string(),
            enabled: true,
        };
        upsert_agent_nostr_config(&conn, &cfg2).unwrap();
        assert_ne!(
            get_agent_nostr_config(&conn, "agent-1")
                .unwrap()
                .unwrap()
                .secret_key,
            get_agent_nostr_config(&conn, "agent-2")
                .unwrap()
                .unwrap()
                .secret_key,
        );

        assert!(delete_agent_nostr_config(&conn, "agent-1").unwrap());
        assert!(get_agent_nostr_config(&conn, "agent-1").unwrap().is_none());
    }
}
