//! セッションに紐づく Nostr 購読（`session_watches` / 載せ替え工程 5-a）。
//!
//! 行があるセッションだけ新機構（束ね / 即時転送）が効く。
//! `interval_secs` は必須・正の整数。未指定・0 以下は拒否する（既定値を埋めない）。
//! `filter_json` は本体 `NostrFilter` と同形の JSON object。空 object は
//! 「その軸では上乗せしない」（現行どおり）。壊れた JSON を空に置き換えない。

use anyhow::{bail, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// `session_watches` の 1 行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWatchRow {
    pub id: i64,
    pub session_id: String,
    pub agent_id: String,
    pub interval_secs: i64,
    pub filter_json: String,
    pub created_at: String,
}

/// `interval_secs` と `filter_json` の書き込み契約。DDL の CHECK と二重に見る。
///
/// 既定値は埋めない。欠けた / 不正な入力は `Err`（fail-loud）。
pub fn validate_watch_write(interval_secs: i64, filter_json: &str) -> Result<()> {
    if interval_secs <= 0 {
        bail!("interval_secs は正の整数が必須（未指定・0 以下は拒否）");
    }
    let value: serde_json::Value = serde_json::from_str(filter_json)
        .map_err(|e| anyhow::anyhow!("filter_json が JSON として読めない: {e}"))?;
    if !value.is_object() {
        bail!("filter_json は JSON object が必須");
    }
    Ok(())
}

/// watch を 1 行足す。`id` を返す。
pub fn insert_session_watch(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    interval_secs: i64,
    filter_json: &str,
) -> Result<i64> {
    validate_watch_write(interval_secs, filter_json)?;
    let created_at = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO session_watches (session_id, agent_id, interval_secs, filter_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, agent_id, interval_secs, filter_json, created_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// `id` で 1 行読む。無ければ `None`。
pub fn get_session_watch(conn: &Connection, id: i64) -> Result<Option<SessionWatchRow>> {
    let result = conn.query_row(
        "SELECT id, session_id, agent_id, interval_secs, filter_json, created_at
         FROM session_watches WHERE id = ?1",
        params![id],
        row_from_watch,
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// この agent の接続で実行する watch を id 順で返す。
pub fn list_session_watches_for_agent(
    conn: &Connection,
    agent_id: &str,
) -> Result<Vec<SessionWatchRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, agent_id, interval_secs, filter_json, created_at
         FROM session_watches WHERE agent_id = ?1 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![agent_id], row_from_watch)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// watch 1 行を更新する。対象が無ければ `false`。
pub fn update_session_watch(
    conn: &Connection,
    id: i64,
    session_id: &str,
    agent_id: &str,
    interval_secs: i64,
    filter_json: &str,
) -> Result<bool> {
    validate_watch_write(interval_secs, filter_json)?;
    let n = conn.execute(
        "UPDATE session_watches
         SET session_id = ?1, agent_id = ?2, interval_secs = ?3, filter_json = ?4
         WHERE id = ?5",
        params![session_id, agent_id, interval_secs, filter_json, id],
    )?;
    Ok(n > 0)
}

/// watch 1 行を消す。対象が無ければ `false`。
pub fn delete_session_watch(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute("DELETE FROM session_watches WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

fn row_from_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionWatchRow> {
    Ok(SessionWatchRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        agent_id: row.get(2)?,
        interval_secs: row.get(3)?,
        filter_json: row.get(4)?,
        created_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        crate::init_memory().expect("init_memory")
    }

    #[test]
    fn insert_requires_positive_interval() {
        let conn = setup();
        let err = insert_session_watch(&conn, "nostr-a", "a", 0, "{}").unwrap_err();
        assert!(
            err.to_string().contains("interval_secs"),
            "0 は拒否: {err:#}"
        );
        let err = insert_session_watch(&conn, "nostr-a", "a", -5, "{}").unwrap_err();
        assert!(
            err.to_string().contains("interval_secs"),
            "負数は拒否: {err:#}"
        );
        assert!(list_session_watches_for_agent(&conn, "a")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn insert_rejects_non_object_filter_json() {
        let conn = setup();
        let err = insert_session_watch(&conn, "nostr-a", "a", 60, "").unwrap_err();
        assert!(
            err.to_string().contains("filter_json"),
            "空文字は拒否: {err:#}"
        );
        let err = insert_session_watch(&conn, "nostr-a", "a", 60, "[]").unwrap_err();
        assert!(
            err.to_string().contains("JSON object"),
            "配列は拒否: {err:#}"
        );
        let err = insert_session_watch(&conn, "nostr-a", "a", 60, "not-json").unwrap_err();
        assert!(
            err.to_string().contains("filter_json"),
            "非 JSON は拒否: {err:#}"
        );
    }

    #[test]
    fn insert_and_list_round_trip() {
        let conn = setup();
        let id = insert_session_watch(
            &conn,
            "nostr-a",
            "a",
            120,
            r#"{"authors":["npub1x"],"keywords":[],"kinds":[1]}"#,
        )
        .unwrap();
        let row = get_session_watch(&conn, id).unwrap().expect("inserted");
        assert_eq!(row.session_id, "nostr-a");
        assert_eq!(row.agent_id, "a");
        assert_eq!(row.interval_secs, 120);
        assert!(row.filter_json.contains("npub1x"));
        assert_eq!(list_session_watches_for_agent(&conn, "a").unwrap().len(), 1);
        assert!(list_session_watches_for_agent(&conn, "other")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn empty_object_filter_is_valid() {
        let conn = setup();
        let id = insert_session_watch(&conn, "nostr-a", "a", 30, "{}").unwrap();
        let row = get_session_watch(&conn, id).unwrap().unwrap();
        assert_eq!(row.filter_json, "{}");
        assert_eq!(row.interval_secs, 30);
    }

    #[test]
    fn multiple_watches_per_session() {
        let conn = setup();
        insert_session_watch(&conn, "nostr-a", "a", 60, "{}").unwrap();
        insert_session_watch(&conn, "nostr-a", "b", 90, "{}").unwrap();
        let rows_a = list_session_watches_for_agent(&conn, "a").unwrap();
        let rows_b = list_session_watches_for_agent(&conn, "b").unwrap();
        assert_eq!(rows_a.len(), 1);
        assert_eq!(rows_b.len(), 1);
        assert_eq!(rows_a[0].agent_id, "a");
        assert_eq!(rows_b[0].agent_id, "b");
    }

    #[test]
    fn update_and_delete() {
        let conn = setup();
        let id = insert_session_watch(&conn, "nostr-a", "a", 60, "{}").unwrap();
        assert!(
            update_session_watch(&conn, id, "nostr-a", "a", 300, r#"{"keywords":["x"]}"#).unwrap()
        );
        let row = get_session_watch(&conn, id).unwrap().unwrap();
        assert_eq!(row.interval_secs, 300);
        assert!(row.filter_json.contains("x"));
        let err = update_session_watch(&conn, id, "nostr-a", "a", 0, "{}").unwrap_err();
        assert!(err.to_string().contains("interval_secs"), "{err:#}");
        assert!(delete_session_watch(&conn, id).unwrap());
        assert!(get_session_watch(&conn, id).unwrap().is_none());
        assert!(!delete_session_watch(&conn, id).unwrap());
    }
}
