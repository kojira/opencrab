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

/// 本鍵（secret_key）だけを差し替える（identity 切替）。他の列は保持する。
pub fn set_agent_nostr_config_secret_key(
    conn: &Connection,
    agent_id: &str,
    secret_key: &str,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_nostr_config SET secret_key = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![secret_key, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

// ---- オーナー識別子（#319） ----
//
// `owner_pubkey` は [`AgentNostrConfigRow`] に**載せない**。この列を読み書きするのは
// 下の 2 関数だけで、[`upsert_agent_nostr_config`] は列名を挙げないので既存値を
// 素通しする。行を丸ごと書き直す経路（REST の設定保存 / 自己ブートストラップの
// 鍵採用 / 鍵生成）が 3 つあり、構造体に載せると**どれか 1 つが空で上書きした瞬間に
// オーナーが消える**（オーナーが黙って居なくなる ＝ 権限が落ちる方向の事故）。
// 読み書きを 1 対の関数へ閉じることで、その事故が構造的に起きない。

/// Nostr 経路のオーナー識別子を読む。行が無ければ空文字（＝オーナー未設定）。
///
/// 戻り値は保存されている表現をそのまま返す（正規化は書き込み口の責務）。
pub fn get_agent_nostr_owner_pubkey(conn: &Connection, agent_id: &str) -> Result<String> {
    let result = conn.query_row(
        "SELECT owner_pubkey FROM agent_nostr_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Nostr 経路のオーナー識別子だけを差し替える。他の列は保持する。
///
/// 空文字を渡すと「オーナー未設定」に戻る（誰もオーナーにならない）。行が無ければ
/// `false`（**行は作らない** — 鍵の無い設定行を副作用で生やさない）。
pub fn set_agent_nostr_owner_pubkey(
    conn: &Connection,
    agent_id: &str,
    owner_pubkey: &str,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_nostr_config SET owner_pubkey = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![owner_pubkey, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

// ---- 逆引き用の自己識別子（#489） ----
//
// `self_pubkey` は [`AgentNostrConfigRow`] に**載せない**（`owner_pubkey` と同じ理由）。
// 読み書きするのは下の 3 関数だけで、[`upsert_agent_nostr_config`] は列名を挙げないので
// 既存値を素通しする。**この列は各 agent 自身の secret_key から導出した pubkey（gateway 起動時）
// と identity 切替時の新 pubkey からしか書かない**のが不変条件で、config 構造体に載せると
// REST の設定保存経由で外部が「pubkey ↔ agent UUID」を仕込めてしまう（#489 の汚染防止）。

/// この agent 自身の Nostr pubkey（64 桁小文字 hex）を保存する（#489）。行が無ければ `false`
/// （**行は作らない** — 鍵の無い設定行を副作用で生やさない）。
///
/// **呼び出してよいのは gateway 起動時の自己 pubkey 導出と identity 切替だけ**（受信イベントの
/// 著者 pubkey からは呼ばない）。
pub fn set_agent_nostr_self_pubkey(
    conn: &Connection,
    agent_id: &str,
    self_pubkey: &str,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_nostr_config SET self_pubkey = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![self_pubkey, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

/// この agent の self_pubkey を読む。行が無ければ空文字（＝未接続 / 逆引き不可）。
pub fn get_agent_nostr_self_pubkey(conn: &Connection, agent_id: &str) -> Result<String> {
    let result = conn.query_row(
        "SELECT self_pubkey FROM agent_nostr_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Nostr pubkey（64 桁小文字 hex）から、その鍵を**自分のもの**として接続した agent の UUID を
/// 逆引きする（#489）。
///
/// `self_pubkey` は各 agent の自 secret_key からしか導出・保存されないので、この対応は
/// **鍵の所有で本人性が担保されている**。空 / 空列は一致させない（fail-closed）。見つからなければ
/// `None`。突き合わせは hex 表現同士で行う前提（呼び出し側が正規化する）。
pub fn resolve_agent_by_nostr_self_pubkey(conn: &Connection, self_pubkey: &str) -> Option<String> {
    if self_pubkey.trim().is_empty() {
        return None;
    }
    conn.query_row(
        "SELECT agent_id FROM agent_nostr_config WHERE self_pubkey = ?1 AND self_pubkey <> ''",
        params![self_pubkey],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Nostr が**設定されているエージェントが 1 つ以上**あるか（enabled は問わない・#620）。
///
/// 起動時にマスターキーの要否を「既存データ」から判定するために使う（設定項目は足さない）。
/// 1 行でもあれば、その行の秘密鍵は暗号化して扱う必要がある＝マスターキーが要る。
pub fn has_any_agent_nostr_config(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM agent_nostr_config", [], |row| {
        row.get(0)
    })?;
    Ok(n > 0)
}

/// **全ての** agent_nostr_config 行（enabled を問わない・#620 の at-rest 移行で使う）。
pub fn list_all_agent_nostr_configs(conn: &Connection) -> Result<Vec<AgentNostrConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, secret_key, relays_json, filter_json, enabled FROM agent_nostr_config",
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

    #[test]
    fn test_set_secret_key_preserves_other_columns() {
        let conn = mem();
        let cfg = AgentNostrConfigRow {
            agent_id: "a1".to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: r#"["wss://yabu.me"]"#.to_string(),
            filter_json: r#"{"keywords":["opencrab"]}"#.to_string(),
            enabled: true,
        };
        upsert_agent_nostr_config(&conn, &cfg).unwrap();
        // 本鍵だけ差し替え、relays/filter/enabled は保持。
        assert!(set_agent_nostr_config_secret_key(&conn, "a1", "nsec1new").unwrap());
        let got = get_agent_nostr_config(&conn, "a1").unwrap().unwrap();
        assert_eq!(got.secret_key, "nsec1new");
        assert_eq!(got.relays_json, r#"["wss://yabu.me"]"#);
        assert_eq!(got.filter_json, r#"{"keywords":["opencrab"]}"#);
        assert!(got.enabled);
        // 未知 agent は false。
        assert!(!set_agent_nostr_config_secret_key(&conn, "missing", "x").unwrap());
    }

    /// #319: オーナー識別子は既定で未設定（空）。設定 → 読み出し → クリアが往復する。
    #[test]
    fn test_owner_pubkey_defaults_to_unset_and_roundtrips() {
        let conn = mem();
        // 行が無ければ空（＝オーナー未設定 / fail-closed）。
        assert_eq!(get_agent_nostr_owner_pubkey(&conn, "a1").unwrap(), "");
        assert!(!set_agent_nostr_owner_pubkey(&conn, "a1", &"1".repeat(64)).unwrap());

        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".to_string(),
                secret_key: "nsec1dummy".to_string(),
                relays_json: "[]".to_string(),
                filter_json: "{}".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        // 行を作っただけではオーナーは居ない。
        assert_eq!(get_agent_nostr_owner_pubkey(&conn, "a1").unwrap(), "");

        let owner = "1".repeat(64);
        assert!(set_agent_nostr_owner_pubkey(&conn, "a1", &owner).unwrap());
        assert_eq!(get_agent_nostr_owner_pubkey(&conn, "a1").unwrap(), owner);

        // 空文字でクリア＝未設定へ戻せる。
        assert!(set_agent_nostr_owner_pubkey(&conn, "a1", "").unwrap());
        assert_eq!(get_agent_nostr_owner_pubkey(&conn, "a1").unwrap(), "");
    }

    /// #489: 自己 pubkey（逆引き用）は既定で未設定（空）。設定 → 読み出し → 逆引きが往復する。
    #[test]
    fn test_self_pubkey_roundtrip_and_reverse_lookup() {
        let conn = mem();
        let pk = "a".repeat(64);
        // 行が無ければ空（＝未接続 / 逆引き不可 / fail-closed）。
        assert_eq!(get_agent_nostr_self_pubkey(&conn, "a1").unwrap(), "");
        assert!(!set_agent_nostr_self_pubkey(&conn, "a1", &pk).unwrap());
        assert!(resolve_agent_by_nostr_self_pubkey(&conn, &pk).is_none());

        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".to_string(),
                secret_key: "nsec1dummy".to_string(),
                relays_json: "[]".to_string(),
                filter_json: "{}".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        // 行を作っただけでは自己 pubkey は空（逆引き不可）。
        assert_eq!(get_agent_nostr_self_pubkey(&conn, "a1").unwrap(), "");
        assert!(resolve_agent_by_nostr_self_pubkey(&conn, &pk).is_none());

        assert!(set_agent_nostr_self_pubkey(&conn, "a1", &pk).unwrap());
        assert_eq!(get_agent_nostr_self_pubkey(&conn, "a1").unwrap(), pk);
        // 逆引きで agent へ戻る。
        assert_eq!(
            resolve_agent_by_nostr_self_pubkey(&conn, &pk).as_deref(),
            Some("a1")
        );
        // 空 pubkey では逆引きしない（空列と偶然一致させない / fail-closed）。
        assert!(resolve_agent_by_nostr_self_pubkey(&conn, "").is_none());
        assert!(resolve_agent_by_nostr_self_pubkey(&conn, "   ").is_none());
        // 登録と違う pubkey は None。
        assert!(resolve_agent_by_nostr_self_pubkey(&conn, &"b".repeat(64)).is_none());
    }

    /// #489: **汚染防止**。self_pubkey は upsert / secret_key 差し替えのどちらでも
    /// 書き換わらない（構造体・upsert 経路から書けないので外部が仕込めない）。
    #[test]
    fn test_upsert_and_secret_key_change_do_not_touch_self_pubkey() {
        let conn = mem();
        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".to_string(),
                secret_key: "nsec1old".to_string(),
                relays_json: "[]".to_string(),
                filter_json: "{}".to_string(),
                enabled: false,
            },
        )
        .unwrap();
        let pk = "c".repeat(64);
        assert!(set_agent_nostr_self_pubkey(&conn, "a1", &pk).unwrap());

        // 行を丸ごと書き直す upsert（鍵差し替え + relays 変更 + 有効化）でも自己 pubkey は不変。
        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".to_string(),
                secret_key: "nsec1new".to_string(),
                relays_json: r#"["wss://relay.example"]"#.to_string(),
                filter_json: r#"{"keywords":["x"]}"#.to_string(),
                enabled: true,
            },
        )
        .unwrap();
        assert_eq!(
            get_agent_nostr_self_pubkey(&conn, "a1").unwrap(),
            pk,
            "upsert が self_pubkey を書き換えた（外部から仕込める穴）"
        );
        // secret_key だけの差し替えでも不変（書き戻しは identity 切替の専用経路が別途行う）。
        set_agent_nostr_config_secret_key(&conn, "a1", "nsec1third").unwrap();
        assert_eq!(get_agent_nostr_self_pubkey(&conn, "a1").unwrap(), pk);
    }

    /// #319: 行を丸ごと書き直す upsert（REST 保存 / 鍵採用 / 鍵生成）で
    /// オーナー識別子が**消えない**。消えると「オーナーが黙って居なくなる」。
    #[test]
    fn test_upsert_preserves_owner_pubkey() {
        let conn = mem();
        let cfg = AgentNostrConfigRow {
            agent_id: "a1".to_string(),
            secret_key: "nsec1old".to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{}".to_string(),
            enabled: false,
        };
        upsert_agent_nostr_config(&conn, &cfg).unwrap();
        let owner = "2".repeat(64);
        assert!(set_agent_nostr_owner_pubkey(&conn, "a1", &owner).unwrap());

        // 別内容で upsert し直す（鍵差し替え + relays 変更 + 有効化）。
        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".to_string(),
                secret_key: "nsec1new".to_string(),
                relays_json: r#"["wss://relay.example"]"#.to_string(),
                filter_json: r#"{"keywords":["x"]}"#.to_string(),
                enabled: true,
            },
        )
        .unwrap();
        assert_eq!(
            get_agent_nostr_owner_pubkey(&conn, "a1").unwrap(),
            owner,
            "upsert がオーナー識別子を消した"
        );
        // 秘密鍵の差し替えでも消えない。
        set_agent_nostr_config_secret_key(&conn, "a1", "nsec1third").unwrap();
        assert_eq!(get_agent_nostr_owner_pubkey(&conn, "a1").unwrap(), owner);
    }
}
