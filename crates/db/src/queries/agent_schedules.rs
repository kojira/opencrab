use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Schedules（per-agent 定時実行 / #455）
// ============================================
//
// cron / `@every` をセッション時刻源へ載せる。既定は無効（fail-closed / #240）。
//
// **語彙・持ち方は heartbeat（`session_heartbeat_config`）に揃える**（v38・#455）:
//   - `last_fired_at`（heartbeat と同名。旧 `last_run_at` を v38 で RENAME）
//   - **次回発火時刻は列に持たず照会時算出**（`next_fire_at` キャッシュ列を作らない）。
//     cron 計算は wake 時のみ・件数も僅少でホットパスに無く、列はキャッシュ無効化漏れ
//     （cron 式/tz/enabled 変更時）による stale リスクだけを増やす。真実は再計算に置く。
// cron 式の検証・発火経路・認可は PR4（scheduler / api）側。

/// 定時実行スケジュール 1 行（`agent_schedules`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScheduleRow {
    /// `None` = 未挿入（INSERT で採番）。
    pub id: Option<i64>,
    pub agent_id: String,
    /// 注入先の一本化されたセッション（Nostr agent は `nostr-{agent}`）。
    pub session_id: String,
    /// 標準 5 フィールド cron、または `@every 3h`。
    pub cron_expr: String,
    /// cron 評価に使う tz（既定 `Asia/Tokyo`）。
    pub timezone: String,
    pub message: String,
    pub enabled: bool,
    /// 発火起点（rfc3339）。`@every` の周期起点、cron では「これ以前のスロットを遡及発火
    /// しない床」。明示の有効化・cron/tz 変更で `now` を打つ（設計 §4.4）。
    pub anchor_at: Option<String>,
    /// 最終発火時刻（rfc3339）。`None` = 未発火。next 計算の base は
    /// `last_fired_at.or(anchor_at)`（heartbeat と同型）。
    pub last_fired_at: Option<String>,
}

fn row_from(row: &rusqlite::Row) -> rusqlite::Result<AgentScheduleRow> {
    Ok(AgentScheduleRow {
        id: Some(row.get(0)?),
        agent_id: row.get(1)?,
        session_id: row.get(2)?,
        cron_expr: row.get(3)?,
        timezone: row.get(4)?,
        message: row.get(5)?,
        enabled: row.get(6)?,
        anchor_at: row.get(7)?,
        last_fired_at: row.get(8)?,
    })
}

const SELECT_COLS: &str =
    "id, agent_id, session_id, cron_expr, timezone, message, enabled, anchor_at, last_fired_at";

/// スケジュールを挿入し、採番された id を返す（`created_at`/`updated_at` は現在時刻）。
pub fn insert_agent_schedule(conn: &Connection, row: &AgentScheduleRow) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO agent_schedules
            (agent_id, session_id, cron_expr, timezone, message, enabled,
             anchor_at, last_fired_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
        params![
            row.agent_id,
            row.session_id,
            row.cron_expr,
            row.timezone,
            row.message,
            row.enabled,
            row.anchor_at,
            row.last_fired_at,
            now,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// id で 1 件取得する。無ければ `None`。
pub fn get_agent_schedule(conn: &Connection, id: i64) -> Result<Option<AgentScheduleRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM agent_schedules WHERE id = ?1");
    let result = conn.query_row(&sql, params![id], row_from);
    match result {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// あるエージェントの全スケジュールを取得する（作成順）。
pub fn list_agent_schedules(conn: &Connection, agent_id: &str) -> Result<Vec<AgentScheduleRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM agent_schedules WHERE agent_id = ?1 ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id], row_from)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// **enabled = 1** のスケジュールを全件列挙する（中央スケジューラ用 / PR4）。
///
/// 発火可否の最終判定（cron/`@every` の next 算出・多重実行防止）はスケジューラ側が握る。
/// ここでは enabled 行を素直に返すだけ（`list_enabled_session_heartbeat_configs` と同じ二段構え）。
/// **G ゲートは掛けない**（schedule は heartbeat のマスタスイッチ G の対象外・自身の enabled で制御）。
pub fn list_enabled_agent_schedules(conn: &Connection) -> Result<Vec<AgentScheduleRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM agent_schedules WHERE enabled = 1 ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 可変フィールドを更新する。`id` 必須。
///
/// `last_fired_at` も含めて上書きする（明示の cron/tz/間隔変更で API 層が `NULL` へ
/// リセットして「新しい式で now 以降の最初のスロットから」始めるため・設計 §4.4）。
pub fn update_agent_schedule(conn: &Connection, row: &AgentScheduleRow) -> Result<()> {
    let Some(id) = row.id else {
        anyhow::bail!("update_agent_schedule: id is required");
    };
    conn.execute(
        "UPDATE agent_schedules SET
            session_id = ?2, cron_expr = ?3, timezone = ?4, message = ?5,
            enabled = ?6, anchor_at = ?7, last_fired_at = ?8, updated_at = ?9
         WHERE id = ?1",
        params![
            id,
            row.session_id,
            row.cron_expr,
            row.timezone,
            row.message,
            row.enabled,
            row.anchor_at,
            row.last_fired_at,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// 発火成功時に `last_fired_at` を進める（スケジューラ用 / PR4）。
///
/// **向き**: `anchor_at` は触らず `last_fired_at` を `now`（引数）へ進める。next は
/// `last_fired + 周期`（`@every`）/ `last_fired` 以降の最初の cron スロットで後ろへ動く
/// （位相を前へ引かない）。実際に発火したときだけ呼ぶ（skip では呼ばない・§6 N2）。
/// **next 時刻はキャッシュしない**（照会時算出）。
pub fn set_agent_schedule_last_fired(
    conn: &Connection,
    id: i64,
    last_fired_at: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_schedules SET last_fired_at = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, last_fired_at, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// スケジュールを削除する。
pub fn delete_agent_schedule(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM agent_schedules WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(agent: &str, enabled: bool) -> AgentScheduleRow {
        AgentScheduleRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: format!("nostr-{agent}"),
            cron_expr: "0 7 * * *".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "毎朝のまとめを書いてください".to_string(),
            enabled,
            anchor_at: Some("2026-08-09T00:00:00+09:00".to_string()),
            last_fired_at: None,
        }
    }

    #[test]
    fn insert_get_list_roundtrip() {
        let conn = crate::init_memory().unwrap();
        let id = insert_agent_schedule(&conn, &sample("a1", true)).unwrap();
        let got = get_agent_schedule(&conn, id).unwrap().unwrap();
        assert_eq!(got.id, Some(id));
        assert_eq!(got.cron_expr, "0 7 * * *");
        assert_eq!(got.last_fired_at, None);
        assert_eq!(list_agent_schedules(&conn, "a1").unwrap().len(), 1);
    }

    #[test]
    fn list_enabled_filters_disabled() {
        let conn = crate::init_memory().unwrap();
        insert_agent_schedule(&conn, &sample("a1", true)).unwrap();
        insert_agent_schedule(&conn, &sample("a2", false)).unwrap();
        let enabled = list_enabled_agent_schedules(&conn).unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].agent_id, "a1");
    }

    #[test]
    fn update_overwrites_and_last_fired_can_reset() {
        let conn = crate::init_memory().unwrap();
        let id = insert_agent_schedule(&conn, &sample("a1", true)).unwrap();
        set_agent_schedule_last_fired(&conn, id, "2026-08-09T07:00:00+09:00").unwrap();
        assert_eq!(
            get_agent_schedule(&conn, id)
                .unwrap()
                .unwrap()
                .last_fired_at,
            Some("2026-08-09T07:00:00+09:00".to_string())
        );

        // 明示の cron 変更 → anchor=now, last_fired=NULL でリセットできる（API 層の方針）。
        let mut row = get_agent_schedule(&conn, id).unwrap().unwrap();
        row.cron_expr = "@every 3h".to_string();
        row.anchor_at = Some("2026-08-09T10:00:00+09:00".to_string());
        row.last_fired_at = None;
        update_agent_schedule(&conn, &row).unwrap();
        let got = get_agent_schedule(&conn, id).unwrap().unwrap();
        assert_eq!(got.cron_expr, "@every 3h");
        assert_eq!(
            got.last_fired_at, None,
            "明示変更で last_fired をリセットできる"
        );
    }

    #[test]
    fn delete_removes_row() {
        let conn = crate::init_memory().unwrap();
        let id = insert_agent_schedule(&conn, &sample("a1", true)).unwrap();
        delete_agent_schedule(&conn, id).unwrap();
        assert!(get_agent_schedule(&conn, id).unwrap().is_none());
    }
}
