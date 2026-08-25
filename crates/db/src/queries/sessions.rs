use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Sessions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub mode: String,
    pub theme: String,
    pub phase: String,
    pub turn_number: i32,
    pub status: String,
    pub participant_ids_json: String,
    pub facilitator_id: Option<String>,
    pub done_count: i32,
    pub max_turns: Option<i32>,
    pub metadata_json: Option<String>,
}

pub fn insert_session(conn: &Connection, session: &SessionRow) -> Result<()> {
    // participant の関係は agent_sessions テーブルが正（#37: インデックス可能・
    // 参照整合な関係表現）。participant_ids_json は web の wire 契約として残す
    // 直列化された投影で、両者はこの単一の挿入点で1トランザクションに書く。
    // 前提: participants は insert 後に変更されない（変更 API は存在しない）。
    // 変更を導入する場合は agent_sessions と JSON の両方を更新すること。
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO sessions (id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            session.id,
            session.mode,
            session.theme,
            session.phase,
            session.turn_number,
            session.status,
            session.participant_ids_json,
            session.facilitator_id,
            session.done_count,
            session.max_turns,
            session.metadata_json,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    if let Ok(serde_json::Value::Array(ids)) =
        serde_json::from_str::<serde_json::Value>(&session.participant_ids_json)
    {
        for id in ids {
            if let Some(agent_id) = id.as_str() {
                tx.execute(
                    "INSERT OR IGNORE INTO agent_sessions (agent_id, session_id) VALUES (?1, ?2)",
                    params![agent_id, session.id],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// セッションの参加エージェント一覧（agent_sessions テーブルが正 — #37）。
pub fn list_session_participants(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    // rowid 順 = 挿入順 = participant_ids_json の配列順（send_message の応答順・
    // 発話順という observable な意味論を旧 JSON 実装から保存する）。
    let mut stmt =
        conn.prepare("SELECT agent_id FROM agent_sessions WHERE session_id = ?1 ORDER BY rowid")?;
    let ids = stmt
        .query_map(params![session_id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// エージェントが参加しているセッション数（agent_sessions テーブルで数える — #37。
/// 旧実装の participant_ids_json への LIKE 部分一致は "a" が "abc" にもマッチした）。
pub fn count_sessions_for_agent(conn: &Connection, agent_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM agent_sessions WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?)
}

/// セッションポリシー（`sessions.policy_json`）。`SessionRow` / 外形 API には載せない。
///
/// 行が無ければ `None`。DEFAULT `'{}'` は「未設定」＝現行挙動（RULINGS Q2）。
/// 欠けたクラスをここで補完しない。
pub fn get_session_policy_json(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT policy_json FROM sessions WHERE id = ?1",
        params![session_id],
        |row| row.get(0),
    );
    match result {
        Ok(json) => Ok(Some(json)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn get_session(conn: &Connection, session_id: &str) -> Result<Option<SessionRow>> {
    let result = conn.query_row(
        "SELECT id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, metadata_json
         FROM sessions WHERE id = ?1",
        params![session_id],
        |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                mode: row.get(1)?,
                theme: row.get(2)?,
                phase: row.get(3)?,
                turn_number: row.get(4)?,
                status: row.get(5)?,
                participant_ids_json: row.get(6)?,
                facilitator_id: row.get(7)?,
                done_count: row.get(8)?,
                max_turns: row.get(9)?,
                metadata_json: row.get(10)?,
            })
        },
    );

    match result {
        Ok(session) => Ok(Some(session)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn list_sessions(conn: &Connection) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, metadata_json
         FROM sessions ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            mode: row.get(1)?,
            theme: row.get(2)?,
            phase: row.get(3)?,
            turn_number: row.get(4)?,
            status: row.get(5)?,
            participant_ids_json: row.get(6)?,
            facilitator_id: row.get(7)?,
            done_count: row.get(8)?,
            max_turns: row.get(9)?,
            metadata_json: row.get(10)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn update_session_metadata(
    conn: &Connection,
    session_id: &str,
    metadata_json: &str,
    theme: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET metadata_json = ?1, theme = ?2, updated_at = ?3 WHERE id = ?4",
        params![metadata_json, theme, Utc::now().to_rfc3339(), session_id],
    )?;
    Ok(())
}

/// `sessions.status` を終端値へ遷移させる（#553）。subtask の死活を永続状態から判定
/// できるようにするため、決着経路（`settle_completed` / `cancel_subtask`）が `exit_reason`
/// 対応の終端値（completed / error / timeout / stopped_by_limit / cancelled）を書く。
/// 存在しない `session_id`（sub-session 行を持たない自動 dispatch など）では 0 行更新で無害。
/// `updated_at` も併せて進める（既存 [`update_session_metadata`] と同型の 1 クエリ）。
pub fn set_session_status(conn: &Connection, session_id: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status, Utc::now().to_rfc3339(), session_id],
    )?;
    Ok(())
}

/// 起動時リコンサイル（#553）: 新プロセスの subtask registry（in-memory）は必ず空なので、
/// この時点で `status='active'` の subtask セッションは**定義上すべて孤児**（前プロセスと
/// 共に実行タスクが消滅済み）。「何分止まったら死」の判定を要せず、確定的に `'interrupted'`
/// へ終端化する。返り値は更新件数。
///
/// 述語 `mode = 'subtask'` は本番実測で `id LIKE 'subtask-%'` と完全一致
/// （active 302/302・any 302/302・不一致 0 行）を確認済み。他モード（autonomous / discord /
/// heartbeat / nostr）には**一切触れない**。`memory_sessions`（会話ログ）にも触れない。
pub fn reconcile_orphaned_subtasks(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE sessions SET status = 'interrupted', updated_at = ?1
         WHERE mode = 'subtask' AND status = 'active'",
        params![Utc::now().to_rfc3339()],
    )?;
    Ok(n)
}

// ============================================
// Heartbeat Log
// ============================================

pub fn insert_heartbeat_log(
    conn: &Connection,
    agent_id: &str,
    decision: &str,
    result_json: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO heartbeat_log (agent_id, decision, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, decision, result_json, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

#[cfg(test)]
mod policy_json_tests {
    use super::*;

    #[test]
    fn new_session_policy_json_is_empty_object() {
        let conn = crate::init_memory().unwrap();
        insert_session(
            &conn,
            &SessionRow {
                id: "nostr-a".into(),
                mode: "nostr".into(),
                theme: "Nostr".into(),
                phase: "active".into(),
                turn_number: 0,
                status: "active".into(),
                participant_ids_json: "[]".into(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
        assert_eq!(
            get_session_policy_json(&conn, "nostr-a")
                .unwrap()
                .as_deref(),
            Some("{}")
        );
        assert_eq!(get_session_policy_json(&conn, "missing").unwrap(), None);
    }
}
