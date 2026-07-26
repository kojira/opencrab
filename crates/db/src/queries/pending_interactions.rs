use anyhow::Result;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

// ============================================
// PENDING INTERACTIONS (A2UI)
// ============================================

#[derive(Debug, Clone)]
pub struct PendingInteractionRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub platform: String,
    pub surface_id: String,
    pub a2ui_components_json: String,
    pub status: String,
    pub response_json: Option<String>,
    pub responder_id: Option<String>,
    pub owner_only: bool,
    pub timeout_secs: i64,
    pub created_at: String,
    pub responded_at: Option<String>,
    pub updated_at: String,
}

/// pending_interactions テーブルへの挿入
pub fn insert_pending_interaction(
    conn: &Connection,
    id: &str,
    agent_id: &str,
    session_id: &str,
    channel_id: &str,
    message_id: Option<&str>,
    platform: &str,
    surface_id: &str,
    a2ui_components_json: &str,
    owner_only: bool,
    timeout_secs: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pending_interactions (id, agent_id, session_id, channel_id, message_id, platform, surface_id, a2ui_components_json, owner_only, timeout_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![id, agent_id, session_id, channel_id, message_id, platform, surface_id, a2ui_components_json, owner_only as i32, timeout_secs],
    )?;
    Ok(())
}

/// 描画済みメッセージ ID を pending_interaction へ書き戻す。
///
/// 送信（描画）は挿入の後に行われるため、`insert_pending_interaction` では
/// `message_id` を埋められない。SQL は移設前（Discord gateway 内の生 SQL / #156 S3）と
/// 同一。
pub fn set_pending_interaction_message_id(
    conn: &Connection,
    id: &str,
    message_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE pending_interactions SET message_id = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![message_id, id],
    )?;
    Ok(())
}

/// pending_interaction の取得
pub fn get_pending_interaction(
    conn: &Connection,
    id: &str,
) -> Result<Option<PendingInteractionRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, session_id, channel_id, message_id, platform, surface_id, a2ui_components_json, status, response_json, responder_id, owner_only, timeout_secs, created_at, responded_at, updated_at
         FROM pending_interactions WHERE id = ?1",
        params![id],
        |row| {
            Ok(PendingInteractionRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                session_id: row.get(2)?,
                channel_id: row.get(3)?,
                message_id: row.get(4)?,
                platform: row.get(5)?,
                surface_id: row.get(6)?,
                a2ui_components_json: row.get(7)?,
                status: row.get(8)?,
                response_json: row.get(9)?,
                responder_id: row.get(10)?,
                owner_only: row.get::<_, i32>(11)? != 0,
                timeout_secs: row.get(12)?,
                created_at: row.get(13)?,
                responded_at: row.get(14)?,
                updated_at: row.get(15)?,
            })
        },
    );
    match result {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// pending_interaction のステータス更新
pub fn update_pending_interaction_status(
    conn: &Connection,
    id: &str,
    status: &str,
    response_json: Option<&str>,
    responder_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE pending_interactions SET status = ?2, response_json = ?3, responder_id = ?4, responded_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
        params![id, status, response_json, responder_id],
    )?;
    Ok(())
}

/// stale pending interactions のクリーンアップ（起動時に呼ぶ）
pub fn cleanup_stale_pending_interactions(conn: &Connection) -> Result<usize> {
    let count = conn.execute(
        "UPDATE pending_interactions SET status = 'timeout', updated_at = datetime('now') WHERE status = 'pending'",
        [],
    )?;
    Ok(count)
}
