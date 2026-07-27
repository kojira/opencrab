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

/// 期限切れとして閉じた保留対話の要約（#196）。
///
/// プロセス（またはゲートウェイ）の再起動でメモリ上の登録簿は消えるため、`pending` の
/// まま残った行は**どこへも応答を返せない**。閉じるときに「どのセッション・どの
/// エージェント・どの描画面だったか」を呼び出し側へ返し、無言で消さずログに残せる
/// ようにする。`session_id` が意味を持つのは #196 で挿入時に埋めるようにしたため。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedInteraction {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub platform: String,
    pub channel_id: String,
    pub surface_id: String,
}

/// `status = 'pending'` の行を列挙する（`agent_id` を渡すとそのエージェント分だけ）。
fn list_pending_interactions(
    conn: &Connection,
    agent_id: Option<&str>,
) -> Result<Vec<ClosedInteraction>> {
    const SELECT: &str = "SELECT id, agent_id, session_id, platform, channel_id, surface_id
         FROM pending_interactions WHERE status = 'pending'";
    let map = |row: &rusqlite::Row<'_>| {
        Ok(ClosedInteraction {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            platform: row.get(3)?,
            channel_id: row.get(4)?,
            surface_id: row.get(5)?,
        })
    };
    let rows: Vec<ClosedInteraction> = match agent_id {
        Some(a) => {
            let mut stmt = conn.prepare(&format!("{SELECT} AND agent_id = ?1"))?;
            let it = stmt.query_map(params![a], map)?;
            it.collect::<std::result::Result<_, _>>()?
        }
        None => {
            let mut stmt = conn.prepare(SELECT)?;
            let it = stmt.query_map([], map)?;
            it.collect::<std::result::Result<_, _>>()?
        }
    };
    Ok(rows)
}

/// stale pending interactions を**期限切れとして明示的に閉じる**（プロセス起動時に呼ぶ）。
///
/// 閉じた行を返す。呼び出し側はこれをログに出すこと（無言で放置しない / #196）。
pub fn cleanup_stale_pending_interactions(conn: &Connection) -> Result<Vec<ClosedInteraction>> {
    let closed = list_pending_interactions(conn, None)?;
    conn.execute(
        "UPDATE pending_interactions SET status = 'timeout', updated_at = datetime('now') WHERE status = 'pending'",
        [],
    )?;
    Ok(closed)
}

/// 指定エージェント分だけ stale pending interactions を閉じる。
///
/// per-agent ゲートウェイの起動は**プロセス起動とは限らない**（ダッシュボードから
/// 実行中に再起動できる）。全件を落とすと、同時に動いている**別エージェントの生きた
/// 保留対話**まで `timeout` にしてしまうため、エージェントで絞る。
pub fn cleanup_stale_pending_interactions_for_agent(
    conn: &Connection,
    agent_id: &str,
) -> Result<Vec<ClosedInteraction>> {
    let closed = list_pending_interactions(conn, Some(agent_id))?;
    conn.execute(
        "UPDATE pending_interactions SET status = 'timeout', updated_at = datetime('now')
         WHERE status = 'pending' AND agent_id = ?1",
        params![agent_id],
    )?;
    Ok(closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        crate::init_memory().unwrap()
    }

    fn insert(conn: &Connection, id: &str, agent: &str, session: &str) {
        insert_pending_interaction(
            conn,
            id,
            agent,
            session,
            "ch-1",
            None,
            "discord",
            "interaction:x",
            "[]",
            true,
            300,
        )
        .unwrap();
    }

    /// 挿入した行から**セッション識別子が引ける**こと（#196 の本体）。
    #[test]
    fn insert_round_trips_session_id() {
        let conn = mem();
        insert(&conn, "i1", "a1", "discord-a1-g-c");
        let row = get_pending_interaction(&conn, "i1").unwrap().unwrap();
        assert_eq!(row.session_id, "discord-a1-g-c");
        assert_eq!(row.status, "pending");
    }

    /// 再起動を模した状況（DB に `pending` の行が残っている）で、閉じた行が
    /// **セッション識別子込みで**返り、状態が `timeout` になること。
    #[test]
    fn cleanup_closes_pending_rows_and_reports_them() {
        let conn = mem();
        insert(&conn, "i1", "a1", "sess-1");
        insert(&conn, "i2", "a2", "sess-2");
        update_pending_interaction_status(&conn, "i2", "responded", None, Some("u1")).unwrap();

        let closed = cleanup_stale_pending_interactions(&conn).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, "i1");
        assert_eq!(closed[0].agent_id, "a1");
        assert_eq!(closed[0].session_id, "sess-1");
        assert_eq!(closed[0].platform, "discord");
        assert_eq!(closed[0].surface_id, "interaction:x");

        assert_eq!(
            get_pending_interaction(&conn, "i1")
                .unwrap()
                .unwrap()
                .status,
            "timeout"
        );
        // 既に応答済みの行は触らない。
        assert_eq!(
            get_pending_interaction(&conn, "i2")
                .unwrap()
                .unwrap()
                .status,
            "responded"
        );
        // 二度目は閉じるものが無い。
        assert!(cleanup_stale_pending_interactions(&conn)
            .unwrap()
            .is_empty());
    }

    /// per-agent ゲートウェイ再起動は**そのエージェント分だけ**閉じる。
    #[test]
    fn cleanup_for_agent_leaves_other_agents_alone() {
        let conn = mem();
        insert(&conn, "i1", "a1", "sess-1");
        insert(&conn, "i2", "a2", "sess-2");

        let closed = cleanup_stale_pending_interactions_for_agent(&conn, "a1").unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, "i1");
        assert_eq!(
            get_pending_interaction(&conn, "i1")
                .unwrap()
                .unwrap()
                .status,
            "timeout"
        );
        assert_eq!(
            get_pending_interaction(&conn, "i2")
                .unwrap()
                .unwrap()
                .status,
            "pending"
        );
    }
}
