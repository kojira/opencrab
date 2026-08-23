use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Impressions
// ============================================
//
// スコープは **agent × target**（#314）。同じ相手なら経路（Discord / Nostr / …）や
// セッションが違っても同じ 1 行を読み書きする。`session_id` 列は「最後に更新された
// セッション」の記録で、絞り込みには使わない。

/// SELECT 句（列順は [`row_to_impression`] と対応させること）。
const IMPRESSION_COLUMNS: &str = "id, agent_id, session_id, target_id, target_name, personality, communication_style, recent_behavior, agreement, notes, last_updated_turn";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpressionRow {
    pub id: String,
    pub agent_id: String,
    /// 最後にこの人物像を更新したセッション（スコープではない）。
    pub session_id: String,
    pub target_id: String,
    pub target_name: String,
    pub personality: String,
    pub communication_style: String,
    pub recent_behavior: String,
    pub agreement: String,
    pub notes: String,
    pub last_updated_turn: i32,
}

fn row_to_impression(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImpressionRow> {
    Ok(ImpressionRow {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        session_id: row.get(2)?,
        target_id: row.get(3)?,
        target_name: row.get(4)?,
        personality: row.get(5)?,
        communication_style: row.get(6)?,
        recent_behavior: row.get(7)?,
        agreement: row.get(8)?,
        notes: row.get(9)?,
        last_updated_turn: row.get(10)?,
    })
}

/// 人物像を書き込む（同じ相手なら経路をまたいで同じ行を更新する）。
///
/// `session_id` / `target_name` も更新対象に含める。前者は「最後に更新した場所」を
/// 最新に保つため、後者は表示名の変更に追従するため。
pub fn upsert_impression(conn: &Connection, imp: &ImpressionRow) -> Result<()> {
    conn.execute(
        "INSERT INTO impressions (id, agent_id, session_id, target_id, target_name, personality, communication_style, recent_behavior, agreement, notes, last_updated_turn, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(agent_id, target_id) DO UPDATE SET
            session_id = excluded.session_id,
            target_name = excluded.target_name,
            personality = excluded.personality,
            communication_style = excluded.communication_style,
            recent_behavior = excluded.recent_behavior,
            agreement = excluded.agreement,
            notes = excluded.notes,
            last_updated_turn = excluded.last_updated_turn,
            updated_at = excluded.updated_at",
        params![
            imp.id,
            imp.agent_id,
            imp.session_id,
            imp.target_id,
            imp.target_name,
            imp.personality,
            imp.communication_style,
            imp.recent_behavior,
            imp.agreement,
            imp.notes,
            imp.last_updated_turn,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// エージェントが持つ人物像を全件返す（セッションで絞らない）。
pub fn get_impressions(conn: &Connection, agent_id: &str) -> Result<Vec<ImpressionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {IMPRESSION_COLUMNS} FROM impressions WHERE agent_id = ?1 ORDER BY target_id"
    ))?;
    let rows = stmt.query_map(params![agent_id], row_to_impression)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 特定の相手の人物像を 1 件引く。無ければ `Ok(None)`。
pub fn get_impression(
    conn: &Connection,
    agent_id: &str,
    target_id: &str,
) -> Result<Option<ImpressionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {IMPRESSION_COLUMNS} FROM impressions WHERE agent_id = ?1 AND target_id = ?2"
    ))?;
    let row = stmt
        .query_row(params![agent_id, target_id], row_to_impression)
        .optional()?;
    Ok(row)
}
