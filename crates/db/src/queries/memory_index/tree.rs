use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// MEMORY INDEX: 階層ツリーノード
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexNodeRow {
    pub id: String,
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub node_type: String,
    pub source_type: String,
    pub title: String,
    pub summary: String,
    pub start_log_id: Option<i64>,
    pub end_log_id: Option<i64>,
    pub source_session_id: Option<String>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub depth: i32,
    pub child_count: i32,
    pub token_count: i32,
    pub created_at: String,
    pub updated_at: String,
    pub short_id: Option<String>,
    /// 検索用キーワードの JSON 配列（例: `["Discord","FTS5"]`）。無ければ `[]`。
    pub keywords_json: String,
    /// 月次ロールアップ（period ノード）の最終要約生成時刻。NULL = 未生成。
    /// `updated_at` は child_count 更新で汚れるため staleness 判定にはこちらを使う。
    pub summary_refreshed_at: Option<String>,
}

/// memory_index_nodes の明示列リスト（positional read は必ずこの順で）。
pub(crate) const INDEX_NODE_COLUMNS: &str = "id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id, keywords_json, summary_refreshed_at";

pub(crate) fn index_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexNodeRow> {
    Ok(IndexNodeRow {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        parent_id: row.get(2)?,
        node_type: row.get(3)?,
        source_type: row.get(4)?,
        title: row.get(5)?,
        summary: row.get(6)?,
        start_log_id: row.get(7)?,
        end_log_id: row.get(8)?,
        source_session_id: row.get(9)?,
        date_from: row.get(10)?,
        date_to: row.get(11)?,
        depth: row.get(12)?,
        child_count: row.get(13)?,
        token_count: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
        short_id: row.get(17)?,
        keywords_json: row.get(18)?,
        summary_refreshed_at: row.get(19)?,
    })
}

/// node 本体と FTS 影テーブルへの複数書き込みを SAVEPOINT で原子化する。
///
/// `unchecked_transaction`（BEGIN）ではなく SAVEPOINT を使うのは、呼び出し元が
/// 既にトランザクション内のことがあるため（例: index_builder::delete_index が
/// tx 内から delete_index_nodes_for_agent を呼ぶ）。BEGIN の入れ子は SQLite が
/// 拒否するが、SAVEPOINT は外側 tx の有無どちらでも動く。
pub(crate) fn with_index_savepoint<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute_batch("SAVEPOINT memory_index_write")?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch("RELEASE memory_index_write")?;
            Ok(v)
        }
        Err(e) => {
            let _ =
                conn.execute_batch("ROLLBACK TO memory_index_write; RELEASE memory_index_write");
            Err(e)
        }
    }
}

/// keywords_json（JSON 配列）を FTS 用の空白区切りテキストに変換する。
fn keywords_fts_text(keywords_json: &str) -> String {
    serde_json::from_str::<Vec<String>>(keywords_json)
        .map(|v| v.join(" "))
        .unwrap_or_default()
}

/// FTS 影テーブルへノードを upsert する（delete + insert）。
///
/// memory_index_nodes への**全ての**テキスト書き込み（insert / summary 更新 /
/// keywords 更新 / rollup）はこの関数を通して FTS と同期すること。
/// トリガーは使わない: v5 マイグレーションの DROP/RENAME 前例でトリガーが
/// 消えるため、既存の memory_sessions_fts と同じ「クエリ層で手動同期」に揃える。
pub(crate) fn fts_upsert_node(
    conn: &Connection,
    node_id: &str,
    agent_id: &str,
    node_type: &str,
    source_type: &str,
    title: &str,
    summary: &str,
    keywords_json: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_fts WHERE node_id = ?1",
        params![node_id],
    )?;
    conn.execute(
        "INSERT INTO memory_index_fts (title, summary, keywords, node_id, agent_id, node_type, source_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            title,
            summary,
            keywords_fts_text(keywords_json),
            node_id,
            agent_id,
            node_type,
            source_type,
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatermarkRow {
    pub agent_id: String,
    pub last_indexed_log_id: i64,
    pub last_indexed_at: String,
    pub total_nodes: i64,
}

#[derive(Debug, Clone)]
pub struct DailyLogWatermarkRow {
    pub agent_id: String,
    pub last_indexed_date: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct DailyLogEntry {
    pub id: String,
    pub agent_id: String,
    pub category: String,
    pub content: String,
    pub date_str: String,
}

pub fn insert_index_node(conn: &Connection, node: &IndexNodeRow) -> Result<()> {
    // 本体と FTS の 2 書き込みを原子化する（insert_session_log と同じ理由:
    // 途中失敗で FTS が恒久欠損すると、OR IGNORE ガードにより二度と修復されない
    // — 検索から見えないノードが残る）。
    with_index_savepoint(conn, |tx| {
        let inserted = tx.execute(
        "INSERT OR IGNORE INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id, keywords_json, summary_refreshed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            node.id,
            node.agent_id,
            node.parent_id,
            node.node_type,
            node.source_type,
            node.title,
            node.summary,
            node.start_log_id,
            node.end_log_id,
            node.source_session_id,
            node.date_from,
            node.date_to,
            node.depth,
            node.child_count,
            node.token_count,
            node.created_at,
            node.updated_at,
            node.short_id,
            node.keywords_json,
            node.summary_refreshed_at,
        ],
    )?;
        // OR IGNORE で既存行が残った場合は FTS も既存のまま（上書きしない）。
        if inserted > 0 {
            fts_upsert_node(
                tx,
                &node.id,
                &node.agent_id,
                &node.node_type,
                &node.source_type,
                &node.title,
                &node.summary,
                &node.keywords_json,
            )?;
        }
        Ok(())
    })
}

/// ノードを1件削除する（FTS 影テーブル・カテゴリ所属・親集計も同期する）。
///
/// memory_index_nodes への生 SQL DELETE は FTS 孤児を残すため禁止 —
/// 必ずこの関数（または `delete_index_nodes_for_agent`）を使うこと。
///
/// parent_id の ON DELETE CASCADE で子孫ノードも一緒に消えるため、FTS と
/// `memory_category_members` は削除**前に**再帰 CTE で部分木全体の id を集めて
/// 同期削除する（非 leaf に対して呼んでも孤児参照を残さない）。
///
/// 後始末は v33 マイグレーション（schema.rs / issue #393）が同じ削除に対して
/// 明示的に実装した意味論をランタイムへ写したもの:
/// - `memory_category_members` … `topic_id` / `category_id` の**両方**でノードを
///   指すため、部分木の id 集合に一致する行を両列で消す（`topic_id` 列名でも
///   unit 等を指す行がありうる）。
/// - 親の `child_count` … 子を失う親を実カウントで直す。索引ビルダの再計算
///   （`index_builder.rs` の `HashMap<parent_id, count>`）は**現存する子を持つ親しか
///   UPDATE しない**ため、最後の子を消すと親の `child_count` は永久にずれる。
///   ここで直さないと自己修復しない。部分木の外にある親は削除対象 `node_id` の親
///   だけ（子孫の親は全て部分木の中で一緒に消える）なので、その 1 件を直せば足りる。
///
/// 掃除・削除・集計直しは同一 savepoint 内で行い、途中失敗で中途半端な状態が
/// 残らないようにする（`with_index_savepoint` がロールバックする）。
pub fn delete_index_node(conn: &Connection, node_id: &str) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        // 子を失う親は削除後に parent_id を辿れないので、削除**前**に控える。
        let parent_id = get_index_node(tx, node_id)?.and_then(|n| n.parent_id);
        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
                SELECT id FROM memory_index_nodes WHERE id = ?1
                UNION ALL
                SELECT n.id FROM memory_index_nodes n JOIN subtree s ON n.parent_id = s.id
             )
             DELETE FROM memory_index_fts WHERE node_id IN (SELECT id FROM subtree)",
            params![node_id],
        )?;
        // カテゴリ所属も同じ部分木集合で消す（宙に浮く参照を残さない / v33 と同じ）。
        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
                SELECT id FROM memory_index_nodes WHERE id = ?1
                UNION ALL
                SELECT n.id FROM memory_index_nodes n JOIN subtree s ON n.parent_id = s.id
             )
             DELETE FROM memory_category_members
             WHERE topic_id IN (SELECT id FROM subtree)
                OR category_id IN (SELECT id FROM subtree)",
            params![node_id],
        )?;
        tx.execute(
            "DELETE FROM memory_index_nodes WHERE id = ?1",
            params![node_id],
        )?;
        // 生き残る親の child_count を実カウントへ直す（子が 0 になる親も含む）。
        if let Some(parent_id) = parent_id {
            tx.execute(
                "UPDATE memory_index_nodes
                 SET child_count = (SELECT COUNT(*) FROM memory_index_nodes c
                                     WHERE c.parent_id = ?1)
                 WHERE id = ?1",
                params![parent_id],
            )?;
        }
        Ok(())
    })
}

/// ノードの keywords_json を更新する（FTS 同期込み）。
pub fn update_index_node_keywords(
    conn: &Connection,
    node_id: &str,
    keywords_json: &str,
) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        tx.execute(
            "UPDATE memory_index_nodes SET keywords_json = ?1, updated_at = ?2 WHERE id = ?3",
            params![keywords_json, Utc::now().to_rfc3339(), node_id],
        )?;
        if let Some(node) = get_index_node(tx, node_id)? {
            fts_upsert_node(
                tx,
                &node.id,
                &node.agent_id,
                &node.node_type,
                &node.source_type,
                &node.title,
                &node.summary,
                &node.keywords_json,
            )?;
        }
        Ok(())
    })
}

pub fn update_index_node_child_count(conn: &Connection, node_id: &str, count: i32) -> Result<()> {
    conn.execute(
        "UPDATE memory_index_nodes SET child_count = ?1, updated_at = ?2 WHERE id = ?3",
        params![count, Utc::now().to_rfc3339(), node_id],
    )?;
    Ok(())
}

pub fn get_index_tree(conn: &Connection, agent_id: &str) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS}
         FROM memory_index_nodes WHERE agent_id = ?1 ORDER BY depth ASC, created_at ASC"
    ))?;
    let rows = stmt.query_map(params![agent_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn get_index_node(conn: &Connection, node_id: &str) -> Result<Option<IndexNodeRow>> {
    let result = conn.query_row(
        &format!("SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes WHERE id = ?1"),
        params![node_id],
        index_node_from_row,
    );
    match result {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn get_daily_log_watermark(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<DailyLogWatermarkRow>> {
    let result = conn.query_row(
        "SELECT agent_id, last_indexed_date, updated_at
         FROM daily_log_index_watermark WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(DailyLogWatermarkRow {
                agent_id: row.get(0)?,
                last_indexed_date: row.get(1)?,
                updated_at: row.get(2)?,
            })
        },
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_daily_log_watermark(conn: &Connection, row: &DailyLogWatermarkRow) -> Result<()> {
    conn.execute(
        "INSERT INTO daily_log_index_watermark (agent_id, last_indexed_date, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(agent_id) DO UPDATE SET
            last_indexed_date = excluded.last_indexed_date,
            updated_at = excluded.updated_at",
        params![row.agent_id, row.last_indexed_date, row.updated_at],
    )?;
    Ok(())
}

pub fn delete_daily_log_watermark(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM daily_log_index_watermark WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

pub fn get_unindexed_daily_logs(
    conn: &Connection,
    agent_id: &str,
    after_date: &str,
) -> Result<Vec<DailyLogEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content
         FROM memory_curated
         WHERE agent_id = ?1
           AND category LIKE 'daily_log/%'
           AND substr(category, 11) > ?2
         ORDER BY category ASC",
    )?;
    let rows = stmt.query_map(params![agent_id, after_date], |row| {
        let category: String = row.get(2)?;
        Ok(DailyLogEntry {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            date_str: category.trim_start_matches("daily_log/").to_string(),
            category,
            content: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn get_daily_log_by_date(
    conn: &Connection,
    agent_id: &str,
    date_str: &str,
) -> Result<Option<DailyLogEntry>> {
    let category = format!("daily_log/{date_str}");
    let result = conn.query_row(
        "SELECT id, agent_id, category, content
         FROM memory_curated
         WHERE agent_id = ?1 AND category = ?2",
        params![agent_id, category],
        |row| {
            let category: String = row.get(2)?;
            Ok(DailyLogEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                date_str: category.trim_start_matches("daily_log/").to_string(),
                category,
                content: row.get(3)?,
            })
        },
    );

    match result {
        Ok(entry) => Ok(Some(entry)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_daily_log_index_node(conn: &Connection, node: &IndexNodeRow) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        tx.execute(
        "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id, keywords_json, summary_refreshed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            summary = excluded.summary,
            updated_at = excluded.updated_at,
            child_count = excluded.child_count",
        params![
            node.id,
            node.agent_id,
            node.parent_id,
            node.node_type,
            node.source_type,
            node.title,
            node.summary,
            node.start_log_id,
            node.end_log_id,
            node.source_session_id,
            node.date_from,
            node.date_to,
            node.depth,
            node.child_count,
            node.token_count,
            node.created_at,
            node.updated_at,
            node.short_id,
            node.keywords_json,
            node.summary_refreshed_at,
        ],
    )?;
        // upsert は title/summary が置き換わりうるので FTS を常に最新へ
        // （既存行の keywords は据え置かれるため現在値を読み直して同期する）。
        if let Some(current) = get_index_node(tx, &node.id)? {
            fts_upsert_node(
                tx,
                &current.id,
                &current.agent_id,
                &current.node_type,
                &current.source_type,
                &current.title,
                &current.summary,
                &current.keywords_json,
            )?;
        }
        Ok(())
    })
}

pub fn get_session_logs_by_id_range(
    conn: &Connection,
    agent_id: &str,
    from_id: i64,
    to_id: i64,
) -> Result<Vec<SessionLogRow>> {
    // #425: 記憶ノードの生ログ全文取得でもエコー行（表示専用）は返さない（記憶系で不可視）。
    let mut stmt = conn.prepare(&format!(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE agent_id = ?1 AND id >= ?2 AND id <= ?3
           AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL} ORDER BY id ASC"
    ))?;
    let rows = stmt.query_map(params![agent_id, from_id, to_id], |row| {
        Ok(SessionLogRow {
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

pub fn get_index_watermark(conn: &Connection, agent_id: &str) -> Result<Option<WatermarkRow>> {
    let result = conn.query_row(
        "SELECT agent_id, last_indexed_log_id, last_indexed_at, total_nodes
         FROM memory_index_watermark WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(WatermarkRow {
                agent_id: row.get(0)?,
                last_indexed_log_id: row.get(1)?,
                last_indexed_at: row.get(2)?,
                total_nodes: row.get(3)?,
            })
        },
    );
    match result {
        Ok(wm) => Ok(Some(wm)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_index_watermark(conn: &Connection, wm: &WatermarkRow) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_index_watermark (agent_id, last_indexed_log_id, last_indexed_at, total_nodes)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
            last_indexed_log_id = excluded.last_indexed_log_id,
            last_indexed_at = excluded.last_indexed_at,
            total_nodes = excluded.total_nodes",
        params![wm.agent_id, wm.last_indexed_log_id, wm.last_indexed_at, wm.total_nodes],
    )?;
    Ok(())
}

pub fn get_unindexed_log_count(conn: &Connection, agent_id: &str) -> Result<i64> {
    let last_id = get_index_watermark(conn, agent_id)?
        .map(|wm| wm.last_indexed_log_id)
        .unwrap_or(0);
    // #425: 未索引件数の見積りからエコー行を除外（記憶材料でないので数に入れない）。
    // 索引ビルダ本体の取得（get_unindexed_session_logs）は**除外しない**——エコー行を跨いで
    // watermark を前進させる必要があるため（エコーだけのバッチが永遠に未索引で詰まらない）。
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM memory_sessions
         WHERE agent_id = ?1 AND id > ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
        ),
        params![agent_id, last_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

pub fn get_unindexed_session_logs(
    conn: &Connection,
    agent_id: &str,
    after_id: i64,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE agent_id = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![agent_id, after_id, limit as i64], |row| {
        Ok(SessionLogRow {
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

/// エージェントの特定 source_type のインデックスノードを削除する（FTS も同期削除）。
/// daily_log ツリーの rebuild 用。
///
/// 前提: 部分木は source_type を跨がない（daily_log ツリーの子孫は全て
/// daily_log）。この前提が崩れると CASCADE で消えた別 source_type の子の
/// FTS 行が残る。
pub fn delete_index_nodes_for_agent_by_source(
    conn: &Connection,
    agent_id: &str,
    source_type: &str,
) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        tx.execute(
            "DELETE FROM memory_index_nodes WHERE agent_id = ?1 AND source_type = ?2",
            params![agent_id, source_type],
        )?;
        tx.execute(
            "DELETE FROM memory_index_fts WHERE agent_id = ?1 AND source_type = ?2",
            params![agent_id, source_type],
        )?;
        Ok(())
    })
}

/// エージェントの全インデックスノードを削除する（FTS 影テーブルも同期削除）
pub fn delete_index_nodes_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        tx.execute(
            "DELETE FROM memory_index_nodes WHERE agent_id = ?1",
            params![agent_id],
        )?;
        tx.execute(
            "DELETE FROM memory_index_fts WHERE agent_id = ?1",
            params![agent_id],
        )?;
        Ok(())
    })
}

/// エージェントのインデックスウォーターマークを削除する
pub fn delete_index_watermark_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_watermark WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

/// インデックスノードのtitle/summaryを更新する（再マージ用、FTS 同期込み）
pub fn update_index_node_summary(
    conn: &Connection,
    node_id: &str,
    title: &str,
    summary: &str,
) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        tx.execute(
            "UPDATE memory_index_nodes SET title = ?1, summary = ?2, updated_at = ?3 WHERE id = ?4",
            params![title, summary, Utc::now().to_rfc3339(), node_id],
        )?;
        if let Some(node) = get_index_node(tx, node_id)? {
            fts_upsert_node(
                tx,
                &node.id,
                &node.agent_id,
                &node.node_type,
                &node.source_type,
                &node.title,
                &node.summary,
                &node.keywords_json,
            )?;
        }
        Ok(())
    })
}
