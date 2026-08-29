use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// 会話圧縮の派生スナップショット 1 行（#826-B）。正本から再構築可能。行追加のみ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSnapshotRow {
    pub id: Option<i64>,
    pub session_id: String,
    pub compacted_conversation: String,
    pub through_log_id: i64,
    pub token_count: i64,
    pub created_at: Option<String>,
}

/// 派生スナップショットを追記する。既存行は更新しない。
pub fn insert_conversation_snapshot(
    conn: &Connection,
    row: &ConversationSnapshotRow,
) -> Result<i64> {
    let created_at = row
        .created_at
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339());
    conn.execute(
        "INSERT INTO conversation_snapshots
         (session_id, compacted_conversation, through_log_id, token_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            row.session_id,
            row.compacted_conversation,
            row.through_log_id,
            row.token_count,
            created_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// session の最新スナップショット（id 最大）。無ければ None。
pub fn latest_conversation_snapshot(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<ConversationSnapshotRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, compacted_conversation, through_log_id, token_count, created_at
         FROM conversation_snapshots
         WHERE session_id = ?1
         ORDER BY id DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(ConversationSnapshotRow {
        id: Some(row.get(0)?),
        session_id: row.get(1)?,
        compacted_conversation: row.get(2)?,
        through_log_id: row.get(3)?,
        token_count: row.get(4)?,
        created_at: Some(row.get(5)?),
    }))
}

/// `through_log_id` より後の正本ログ（id ASC）。スナップショット差分の組立用。
pub fn list_session_logs_after(
    conn: &Connection,
    session_id: &str,
    through_log_id: i64,
) -> Result<Vec<super::SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions
         WHERE session_id = ?1 AND id > ?2
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![session_id, through_log_id], |row| {
        Ok(super::SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_memory;

    #[test]
    fn insert_is_append_only_and_latest_is_newest() {
        let conn = init_memory().unwrap();
        let first = ConversationSnapshotRow {
            id: None,
            session_id: "s1".into(),
            compacted_conversation: "first".into(),
            through_log_id: 3,
            token_count: 10,
            created_at: None,
        };
        let id1 = insert_conversation_snapshot(&conn, &first).unwrap();
        let second = ConversationSnapshotRow {
            id: None,
            session_id: "s1".into(),
            compacted_conversation: "second".into(),
            through_log_id: 9,
            token_count: 8,
            created_at: None,
        };
        let id2 = insert_conversation_snapshot(&conn, &second).unwrap();
        assert!(id2 > id1);
        let latest = latest_conversation_snapshot(&conn, "s1").unwrap().unwrap();
        assert_eq!(latest.compacted_conversation, "second");
        assert_eq!(latest.through_log_id, 9);
        assert_eq!(latest.token_count, 8);
        assert!(latest_conversation_snapshot(&conn, "missing")
            .unwrap()
            .is_none());
    }
}
