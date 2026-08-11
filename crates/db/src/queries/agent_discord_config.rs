use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Discord Config
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDiscordConfigRow {
    pub agent_id: String,
    pub bot_token: String,
    pub owner_discord_id: String,
    pub enabled: bool,
}

pub fn upsert_agent_discord_config(conn: &Connection, cfg: &AgentDiscordConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_discord_config (agent_id, bot_token, owner_discord_id, enabled, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
            bot_token = excluded.bot_token,
            owner_discord_id = excluded.owner_discord_id,
            enabled = excluded.enabled,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.bot_token,
            cfg.owner_discord_id,
            cfg.enabled,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_agent_discord_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<AgentDiscordConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, bot_token, owner_discord_id, enabled
         FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentDiscordConfigRow {
                agent_id: row.get(0)?,
                bot_token: row.get(1)?,
                owner_discord_id: row.get(2)?,
                enabled: row.get(3)?,
            })
        },
    );

    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_agent_discord_config(conn: &Connection, agent_id: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(deleted > 0)
}

pub fn set_agent_discord_config_enabled(
    conn: &Connection,
    agent_id: &str,
    enabled: bool,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_discord_config SET enabled = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![enabled, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

pub fn patch_agent_discord_config(
    conn: &Connection,
    agent_id: &str,
    bot_token: Option<&str>,
    owner_discord_id: Option<&str>,
) -> Result<bool> {
    let updated = match (bot_token, owner_discord_id) {
        (Some(token), Some(owner)) => conn.execute(
            "UPDATE agent_discord_config SET bot_token = ?1, owner_discord_id = ?2, updated_at = ?3 WHERE agent_id = ?4",
            params![token, owner, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (Some(token), None) => conn.execute(
            "UPDATE agent_discord_config SET bot_token = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![token, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (None, Some(owner)) => conn.execute(
            "UPDATE agent_discord_config SET owner_discord_id = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![owner, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (None, None) => 0,
    };
    Ok(updated > 0)
}

// ---- 逆引き用の自己識別子（#489） ----
//
// `bot_user_id` は [`AgentDiscordConfigRow`] に**載せない**。読み書きするのは下の 3 関数だけで、
// [`upsert_agent_discord_config`] / [`patch_agent_discord_config`] は列名を挙げないので既存値を
// 素通しする。**この列は各 bot 自身の認証済み接続（`get_current_user`）からしか書かない**
// のが不変条件で、config 構造体に載せると REST の設定保存経由で外部が「Discord user_id ↔
// agent UUID」を仕込めてしまい、任意ユーザーが co_agent に化ける（#489 の汚染防止）。

/// この agent の bot 自身の Discord user id を保存する（#489）。行が無ければ `false`
/// （**行は作らない** — bot_token の無い設定行を副作用で生やさない）。
///
/// **呼び出してよいのは gateway 起動時の自己接続だけ**（受信メッセージの author からは呼ばない）。
pub fn set_agent_discord_bot_user_id(
    conn: &Connection,
    agent_id: &str,
    bot_user_id: &str,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_discord_config SET bot_user_id = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![bot_user_id, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

/// この agent の bot_user_id を読む。行が無ければ空文字（＝未接続 / 逆引き不可）。
pub fn get_agent_discord_bot_user_id(conn: &Connection, agent_id: &str) -> Result<String> {
    let result = conn.query_row(
        "SELECT bot_user_id FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Discord user id から、その id を**自分のもの**として接続した agent の UUID を逆引きする（#489）。
///
/// `bot_user_id` は各 bot の自己接続からしか書かれないので、この対応は**接続で本人性が
/// 担保されている**。空 id / 空列は一致させない（fail-closed）。見つからなければ `None`。
pub fn resolve_agent_by_discord_bot_user_id(
    conn: &Connection,
    bot_user_id: &str,
) -> Option<String> {
    if bot_user_id.trim().is_empty() {
        return None;
    }
    conn.query_row(
        "SELECT agent_id FROM agent_discord_config WHERE bot_user_id = ?1 AND bot_user_id <> ''",
        params![bot_user_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

pub fn list_enabled_agent_discord_configs(conn: &Connection) -> Result<Vec<AgentDiscordConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, bot_token, owner_discord_id, enabled
         FROM agent_discord_config WHERE enabled = 1",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(AgentDiscordConfigRow {
            agent_id: row.get(0)?,
            bot_token: row.get(1)?,
            owner_discord_id: row.get(2)?,
            enabled: row.get(3)?,
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

    fn row(agent_id: &str) -> AgentDiscordConfigRow {
        AgentDiscordConfigRow {
            agent_id: agent_id.to_string(),
            bot_token: "tok".to_string(),
            owner_discord_id: "owner".to_string(),
            enabled: true,
        }
    }

    /// #489: bot_user_id（逆引き用）は既定で空。設定 → 読み出し → 逆引きが往復する。
    #[test]
    fn test_bot_user_id_roundtrip_and_reverse_lookup() {
        let conn = mem();
        // 行が無ければ空（＝未接続 / 逆引き不可 / fail-closed）。
        assert_eq!(get_agent_discord_bot_user_id(&conn, "a1").unwrap(), "");
        assert!(!set_agent_discord_bot_user_id(&conn, "a1", "123").unwrap());
        assert!(resolve_agent_by_discord_bot_user_id(&conn, "123").is_none());

        upsert_agent_discord_config(&conn, &row("a1")).unwrap();
        // 行を作っただけでは bot_user_id は空（逆引き不可）。
        assert_eq!(get_agent_discord_bot_user_id(&conn, "a1").unwrap(), "");
        assert!(resolve_agent_by_discord_bot_user_id(&conn, "123").is_none());

        assert!(set_agent_discord_bot_user_id(&conn, "a1", "123").unwrap());
        assert_eq!(get_agent_discord_bot_user_id(&conn, "a1").unwrap(), "123");
        assert_eq!(
            resolve_agent_by_discord_bot_user_id(&conn, "123").as_deref(),
            Some("a1")
        );
        // 空 id では逆引きしない（空列と偶然一致させない / fail-closed）。
        assert!(resolve_agent_by_discord_bot_user_id(&conn, "").is_none());
        assert!(resolve_agent_by_discord_bot_user_id(&conn, "   ").is_none());
        // 登録と違う id は None。
        assert!(resolve_agent_by_discord_bot_user_id(&conn, "999").is_none());
    }

    /// #489: **汚染防止**。bot_user_id は upsert / patch のどちらでも書き換わらない
    /// （構造体・設定書き込み経路から書けないので外部が仕込めない）。
    #[test]
    fn test_upsert_and_patch_do_not_touch_bot_user_id() {
        let conn = mem();
        upsert_agent_discord_config(&conn, &row("a1")).unwrap();
        assert!(set_agent_discord_bot_user_id(&conn, "a1", "123").unwrap());

        // 行を丸ごと書き直す upsert（token / owner / enabled 変更）でも bot_user_id は不変。
        upsert_agent_discord_config(
            &conn,
            &AgentDiscordConfigRow {
                agent_id: "a1".to_string(),
                bot_token: "tok2".to_string(),
                owner_discord_id: "owner2".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        assert_eq!(
            get_agent_discord_bot_user_id(&conn, "a1").unwrap(),
            "123",
            "upsert が bot_user_id を書き換えた（外部から仕込める穴）"
        );

        // patch（token/owner のみ）でも不変。
        patch_agent_discord_config(&conn, "a1", Some("tok3"), Some("owner3")).unwrap();
        assert_eq!(get_agent_discord_bot_user_id(&conn, "a1").unwrap(), "123");
    }
}
