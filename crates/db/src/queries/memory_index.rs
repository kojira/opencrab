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
fn with_index_savepoint<T>(
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
fn fts_upsert_node(
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

/// ノードを1件削除する（FTS 影テーブルも同期削除）。
///
/// memory_index_nodes への生 SQL DELETE は FTS 孤児を残すため禁止 —
/// 必ずこの関数（または `delete_index_nodes_for_agent`）を使うこと。
///
/// parent_id の ON DELETE CASCADE で子孫ノードも一緒に消えるため、FTS 側は
/// 削除**前に**再帰 CTE で部分木全体の id を集めて同期削除する（非 leaf に
/// 対して呼んでも FTS 孤児を残さない）。
pub fn delete_index_node(conn: &Connection, node_id: &str) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        tx.execute(
            "WITH RECURSIVE subtree(id) AS (
                SELECT id FROM memory_index_nodes WHERE id = ?1
                UNION ALL
                SELECT n.id FROM memory_index_nodes n JOIN subtree s ON n.parent_id = s.id
             )
             DELETE FROM memory_index_fts WHERE node_id IN (SELECT id FROM subtree)",
            params![node_id],
        )?;
        tx.execute(
            "DELETE FROM memory_index_nodes WHERE id = ?1",
            params![node_id],
        )?;
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
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE agent_id = ?1 AND id >= ?2 AND id <= ?3 ORDER BY id ASC",
    )?;
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
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = ?1 AND id > ?2",
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

// ============================================
// 月次ロールアップ / 常時注入セクション用クエリ
// ============================================

/// INDEX_NODE_COLUMNS にテーブルエイリアスの接頭辞を付ける（JOIN 用）。
fn prefixed_index_node_columns(alias: &str) -> String {
    format!(
        "{alias}.{}",
        INDEX_NODE_COLUMNS.replace(", ", &format!(", {alias}."))
    )
}

/// ロールアップが必要な過去月の period ノードを 1 件返す（最古優先）。
///
/// stale の定義: 配下（period→session→topic）に topic があり、かつ
/// 「未ロールアップ（summary_refreshed_at IS NULL）」または
/// 「ロールアップ後に作られた topic がある（topic.created_at > refreshed_at）」。
/// updated_at ではなく created_at 基準にするのは、keywords バックフィルの
/// UPDATE で再ロールアップを発火させないため。現在月は対象外（注入側で
/// topic 粒度のまま見せるため、ロールアップは無駄撃ちになる）。
pub fn find_stale_period(conn: &Connection, agent_id: &str) -> Result<Option<IndexNodeRow>> {
    let sql = format!(
        "SELECT {cols} FROM memory_index_nodes p
         WHERE p.agent_id = ?1 AND p.node_type = 'period' AND p.source_type = 'session_log'
           AND p.title < strftime('%Y-%m', 'now')
           AND EXISTS (
               SELECT 1 FROM memory_index_nodes s
               JOIN memory_index_nodes t ON t.parent_id = s.id
               WHERE s.parent_id = p.id AND t.node_type = 'topic'
           )
           AND (
               p.summary_refreshed_at IS NULL
               OR EXISTS (
                   SELECT 1 FROM memory_index_nodes s2
                   JOIN memory_index_nodes t2 ON t2.parent_id = s2.id
                   WHERE s2.parent_id = p.id AND t2.node_type = 'topic'
                     AND t2.created_at > p.summary_refreshed_at
               )
           )
         ORDER BY p.title ASC LIMIT 1",
        cols = prefixed_index_node_columns("p"),
    );
    let result = conn.query_row(&sql, params![agent_id], index_node_from_row);
    match result {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// period 配下の topic 一覧（period→session→topic の孫）を時系列順で返す。
pub fn list_topics_for_period(
    conn: &Connection,
    agent_id: &str,
    period_id: &str,
) -> Result<Vec<IndexNodeRow>> {
    let sql = format!(
        "SELECT {cols} FROM memory_index_nodes t
         JOIN memory_index_nodes s ON t.parent_id = s.id
         WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND s.parent_id = ?2
         ORDER BY t.created_at ASC",
        cols = prefixed_index_node_columns("t"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id, period_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 月次ロールアップの結果を period ノードへ書き込む（FTS 同期込み）。
/// summary_refreshed_at を刻むことで find_stale_period の対象から外れる。
pub fn update_period_rollup(
    conn: &Connection,
    node_id: &str,
    summary: &str,
    keywords_json: &str,
) -> Result<()> {
    with_index_savepoint(conn, |tx| {
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE memory_index_nodes
             SET summary = ?1, keywords_json = ?2, summary_refreshed_at = ?3, updated_at = ?3
             WHERE id = ?4",
            params![summary, keywords_json, now, node_id],
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

/// period ノード id → 配下（period→session→topic）の topic 総数。
/// period.child_count は直下の **session** 数なので、月行の「N topics」表示には
/// こちらを使う。
pub fn count_topics_per_period(
    conn: &Connection,
    agent_id: &str,
) -> Result<std::collections::HashMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT s.parent_id, COUNT(*) FROM memory_index_nodes t
         JOIN memory_index_nodes s ON t.parent_id = s.id
         WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND s.parent_id IS NOT NULL
         GROUP BY s.parent_id",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// session_log ツリーの period（月）ノード一覧を新しい月から順に返す。
pub fn list_period_nodes(conn: &Connection, agent_id: &str) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'period' AND source_type = 'session_log'
         ORDER BY title DESC"
    ))?;
    let rows = stmt.query_map(params![agent_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 指定月（date_from が `YYYY-MM` 始まり）の topic を新しい順に返す。
/// `exclude_session_id` のセッション由来 topic は除外（現セッションの topic は
/// コンパクション時の [Past context summary] が担当 — short_id の重複を避ける）。
/// source_session_id が NULL の topic（merge_topics 産）は含める。
pub fn list_topic_nodes_for_month(
    conn: &Connection,
    agent_id: &str,
    month_prefix: &str,
    exclude_session_id: &str,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'topic' AND source_type = 'session_log'
           AND date_from LIKE ?2 || '%'
           AND (source_session_id IS NULL OR source_session_id != ?3)
         ORDER BY created_at DESC LIMIT ?4"
    ))?;
    let rows = stmt.query_map(
        params![agent_id, month_prefix, exclude_session_id, limit as i64],
        index_node_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 未インデックスログの件数と最新ログの created_at を返す（メンテナンスの
/// アイドルゲート用: 「会話が続いている最中」のビルドを避ける）。
pub fn get_unindexed_stats(conn: &Connection, agent_id: &str) -> Result<(i64, Option<String>)> {
    let last_id = get_index_watermark(conn, agent_id)?
        .map(|wm| wm.last_indexed_log_id)
        .unwrap_or(0);
    Ok(conn.query_row(
        "SELECT COUNT(*), MAX(created_at) FROM memory_sessions WHERE agent_id = ?1 AND id > ?2",
        params![agent_id, last_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

// ============================================
// エージェント別メモリインデックス設定
// ============================================

/// 定数: 最小値ガード
pub const BATCH_SIZE_MIN: i64 = 10;
pub const THRESHOLD_MIN: i64 = 5;
pub const BATCH_SIZE_DEFAULT: i64 = 50;
pub const THRESHOLD_DEFAULT: i64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMemoryIndexConfig {
    pub agent_id: String,
    pub batch_size: i64,
    pub threshold: i64,
    pub updated_at: String,
}

/// エージェントのメモリインデックス設定を取得（なければデフォルト値を返す）
pub fn get_memory_index_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<AgentMemoryIndexConfig> {
    let result = conn.query_row(
        "SELECT agent_id, batch_size, threshold, updated_at FROM agent_memory_index_config WHERE agent_id = ?1",
        rusqlite::params![agent_id],
        |row| {
            Ok(AgentMemoryIndexConfig {
                agent_id: row.get(0)?,
                batch_size: row.get(1)?,
                threshold: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    );

    match result {
        Ok(config) => Ok(config),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(AgentMemoryIndexConfig {
            agent_id: agent_id.to_string(),
            batch_size: BATCH_SIZE_DEFAULT,
            threshold: THRESHOLD_DEFAULT,
            updated_at: chrono::Utc::now().to_rfc3339(),
        }),
        Err(e) => Err(e.into()),
    }
}

/// エージェントのメモリインデックス設定を更新（最小値ガード付き）
pub fn upsert_memory_index_config(
    conn: &Connection,
    agent_id: &str,
    batch_size: i64,
    threshold: i64,
) -> Result<AgentMemoryIndexConfig> {
    let batch_size = batch_size.max(BATCH_SIZE_MIN);
    let threshold = threshold.max(THRESHOLD_MIN);
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO agent_memory_index_config (agent_id, batch_size, threshold, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
             batch_size = excluded.batch_size,
             threshold = excluded.threshold,
             updated_at = excluded.updated_at",
        rusqlite::params![agent_id, batch_size, threshold, now],
    )?;

    Ok(AgentMemoryIndexConfig {
        agent_id: agent_id.to_string(),
        batch_size,
        threshold,
        updated_at: now,
    })
}

/// スリープ棚卸しの最終実行時刻を取得する。行が無い/NULL なら `None`。
///
/// `get_memory_index_config` は行が無いとき非永続デフォルトを返す（行を作らない）ため、
/// 棚卸し状態はこの専用 getter/setter で明示的に読み書きする
/// （design-sleep-skill-consolidation.md §5/§8.3）。
pub fn get_last_skill_consolidation_at(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT last_skill_consolidation_at FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// スリープ棚卸しの最終実行時刻を UPSERT で永続化する（行が無ければ作る）。
/// config 行は自動生成されないため、初回シード/実行後にこれで明示的に刻む。
pub fn set_last_skill_consolidation_at(conn: &Connection, agent_id: &str, ts: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, last_skill_consolidation_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             last_skill_consolidation_at = excluded.last_skill_consolidation_at",
        params![
            agent_id,
            BATCH_SIZE_DEFAULT,
            THRESHOLD_DEFAULT,
            chrono::Utc::now().to_rfc3339(),
            ts,
        ],
    )?;
    Ok(())
}

/// スリープ整理ラン（#313 段階3）の最終実行時刻を取得する。行が無い/NULL なら `None`。
///
/// `last_skill_consolidation_at` と同型のマーカー。整理ランはこれを 2 つの用途に使う:
/// (1) 日次ゲート（`now - last_organize_at >= 間隔`）、(2) bounded worklist の下端
/// （このマーカー以降に作られた topic だけを整理対象にする）。`None`（初回遭遇）は
/// 呼び出し側が `now` をシードして 1 回スキップする（既存の全 topic を一気に対象化しない）。
pub fn get_last_organize_at(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT last_organize_at FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// スリープ整理ランの最終実行時刻を UPSERT で永続化する（行が無ければ作る）。
/// config 行は自動生成されないため、初回シード/整理ラン後にこれで明示的に刻む。
pub fn set_last_organize_at(conn: &Connection, agent_id: &str, ts: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, last_organize_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             last_organize_at = excluded.last_organize_at",
        params![
            agent_id,
            BATCH_SIZE_DEFAULT,
            THRESHOLD_DEFAULT,
            chrono::Utc::now().to_rfc3339(),
            ts,
        ],
    )?;
    Ok(())
}

/// スリープ整理ラン（#313 段階3）の worklist 対象 topic 数を数える（発火の下限ゲート用）。
///
/// 対象 = `node_type='topic'` かつ `source_type='session_log'` で、
/// (a) `since`（前回マーカー）より後に作られ、(b) スナップショット `snapshot_log_id`
/// （`memory_index_watermark.last_indexed_log_id`）以下に収まっているもの。`since=None`
/// なら下端制約なし。`end_log_id IS NULL` の topic はスナップショット内とみなす。
pub fn count_organize_topics(
    conn: &Connection,
    agent_id: &str,
    since: Option<&str>,
    snapshot_log_id: i64,
) -> Result<i64> {
    let n = conn.query_row(
        "SELECT COUNT(*) FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND (?2 IS NULL OR n.created_at > ?2)
           AND (n.end_log_id IS NULL OR n.end_log_id <= ?3)",
        params![agent_id, since, snapshot_log_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(n)
}

/// スリープ整理ランの worklist（対象 topic を古い順で最大 `limit` 件）を返す。
///
/// フィルタは [`count_organize_topics`] と同一。古い順（`created_at ASC`）なので、
/// `limit` で切ったときの残り（＝より新しい topic）は次回のマーカー前進後に自然と拾える
/// （前進のみ / 残りは次回）。
pub fn list_organize_topics(
    conn: &Connection,
    agent_id: &str,
    since: Option<&str>,
    snapshot_log_id: i64,
    limit: i64,
) -> Result<Vec<IndexNodeRow>> {
    let sql = format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND (?2 IS NULL OR n.created_at > ?2)
           AND (n.end_log_id IS NULL OR n.end_log_id <= ?3)
         ORDER BY n.created_at ASC LIMIT ?4"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![agent_id, since, snapshot_log_id, limit],
        index_node_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn next_short_id(conn: &Connection, agent_id: &str, prefix: &str) -> Result<String> {
    let max: Option<i64> = conn
        .query_row(
            "SELECT MAX(CAST(SUBSTR(short_id, ?3) AS INTEGER)) FROM memory_index_nodes WHERE agent_id = ?1 AND short_id LIKE ?2",
            params![agent_id, format!("{prefix}%"), (prefix.len() + 1) as i64],
            |row| row.get(0),
        )
        .unwrap_or(None);
    Ok(format!("{prefix}{}", max.unwrap_or(0) + 1))
}

pub fn backfill_short_ids(conn: &Connection) -> Result<usize> {
    let agent_ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT agent_id FROM memory_index_nodes WHERE short_id IS NULL")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut total = 0usize;
    for agent_id in &agent_ids {
        let nodes: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, node_type FROM memory_index_nodes WHERE agent_id = ?1 AND short_id IS NULL ORDER BY created_at ASC"
            )?;
            let rows = stmt.query_map(params![agent_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (node_id, node_type) in &nodes {
            let prefix = match node_type.as_str() {
                "topic" => "t",
                "period" => "p",
                "daily" => "d",
                "session" => "s",
                "hourly" => "h",
                "weekly" => "w",
                "monthly" => "m",
                "yearly" => "y",
                "root" => "r",
                "category" => "c",
                "meta" => "g",
                _ => "x",
            };
            let sid = next_short_id(conn, agent_id, prefix)?;
            conn.execute(
                "UPDATE memory_index_nodes SET short_id = ?1 WHERE id = ?2",
                params![sid, node_id],
            )?;
            total += 1;
        }
    }
    Ok(total)
}

pub fn get_index_node_by_short_or_id(
    conn: &Connection,
    agent_id: &str,
    query: &str,
) -> Result<Option<IndexNodeRow>> {
    let result = conn.query_row(
        &format!(
            "SELECT {INDEX_NODE_COLUMNS}
             FROM memory_index_nodes WHERE agent_id = ?1 AND short_id = ?2"
        ),
        params![agent_id, query],
        index_node_from_row,
    );
    match result {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // フルIDでのフォールバック検索も agent_id でスコープする。
            // スコープしないと他エージェントのノード（非公開会話のタイトル/サマリ）が
            // 予測可能なID経由で漏洩する。
            match get_index_node(conn, query)? {
                Some(node) if node.agent_id == agent_id => Ok(Some(node)),
                _ => Ok(None),
            }
        }
        Err(e) => Err(e.into()),
    }
}

// ============================================
// ノード検索（キーワード逆引き）
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexNodeSearchResult {
    pub node_id: String,
    pub short_id: Option<String>,
    pub node_type: String,
    pub source_type: String,
    pub title: String,
    pub summary: String,
    pub keywords_json: String,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub child_count: i32,
    pub score: f64,
}

/// キーワード/タイトル/要約でインデックスノードを BM25 検索する（逆引き）。
///
/// トークンは引用符でエスケープして AND 結合。0 件なら OR 結合で再検索して
/// リコールを稼ぐ（LLM が打つ複合クエリは全語一致しないことが多い）。
/// FTS は trigram トークナイザ（3 文字以上の部分一致）なので、それでも 0 件
/// かつ短い語を含むクエリは LIKE スキャンにフォールバックする（ノード表は
/// 高々数千行なので全走査でも安価）。
pub fn search_index_nodes(
    conn: &Connection,
    agent_id: &str,
    query: &str,
    limit: usize,
    node_type: Option<&str>,
) -> Result<Vec<IndexNodeSearchResult>> {
    let raw_tokens: Vec<&str> = query.split_whitespace().collect();
    if raw_tokens.is_empty() {
        return Ok(Vec::new());
    }
    let tokens: Vec<String> = raw_tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    let and_query = tokens.join(" AND ");
    let results = search_index_nodes_fts(conn, agent_id, &and_query, limit, node_type)?;
    if !results.is_empty() {
        return Ok(results);
    }
    if tokens.len() > 1 {
        let or_query = tokens.join(" OR ");
        let results = search_index_nodes_fts(conn, agent_id, &or_query, limit, node_type)?;
        if !results.is_empty() {
            return Ok(results);
        }
    }
    // trigram は 3 文字未満の語に当たらない。短い語を含む場合のみ LIKE で救済。
    if raw_tokens.iter().any(|t| t.chars().count() < 3) {
        return search_index_nodes_like(conn, agent_id, &raw_tokens, limit, node_type);
    }
    Ok(Vec::new())
}

fn search_index_nodes_like(
    conn: &Connection,
    agent_id: &str,
    tokens: &[&str],
    limit: usize,
    node_type: Option<&str>,
) -> Result<Vec<IndexNodeSearchResult>> {
    // いずれかの語が title/summary/keywords に部分一致すれば拾う（OR 相当）。
    // LIKE のメタ文字はエスケープする。
    let mut conditions = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params_vec.push(Box::new(agent_id.to_string()));
    for token in tokens {
        let escaped = token
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let idx = params_vec.len() + 1;
        conditions.push(format!(
            "(title LIKE ?{idx} ESCAPE '\\' OR summary LIKE ?{idx} ESCAPE '\\' OR keywords_json LIKE ?{idx} ESCAPE '\\')"
        ));
        params_vec.push(Box::new(pattern));
    }
    let type_idx = params_vec.len() + 1;
    params_vec.push(Box::new(node_type.map(|s| s.to_string())));
    let limit_idx = params_vec.len() + 1;
    params_vec.push(Box::new(limit as i64));

    let sql = format!(
        "SELECT id, short_id, node_type, source_type, title, summary,
                keywords_json, date_from, date_to, child_count, 0.0 as score
         FROM memory_index_nodes
         WHERE agent_id = ?1 AND ({})
           AND (?{type_idx} IS NULL OR node_type = ?{type_idx})
         ORDER BY created_at DESC LIMIT ?{limit_idx}",
        conditions.join(" OR ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok(IndexNodeSearchResult {
            node_id: row.get(0)?,
            short_id: row.get(1)?,
            node_type: row.get(2)?,
            source_type: row.get(3)?,
            title: row.get(4)?,
            summary: row.get(5)?,
            keywords_json: row.get(6)?,
            date_from: row.get(7)?,
            date_to: row.get(8)?,
            child_count: row.get(9)?,
            score: row.get(10)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn search_index_nodes_fts(
    conn: &Connection,
    agent_id: &str,
    fts_query: &str,
    limit: usize,
    node_type: Option<&str>,
) -> Result<Vec<IndexNodeSearchResult>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.short_id, n.node_type, n.source_type, n.title, n.summary,
                n.keywords_json, n.date_from, n.date_to, n.child_count,
                bm25(memory_index_fts) as score
         FROM memory_index_fts fts
         JOIN memory_index_nodes n ON fts.node_id = n.id
         WHERE fts.agent_id = ?1 AND memory_index_fts MATCH ?2
           AND (?4 IS NULL OR n.node_type = ?4)
         ORDER BY score
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![agent_id, fts_query, limit as i64, node_type],
        |row| {
            Ok(IndexNodeSearchResult {
                node_id: row.get(0)?,
                short_id: row.get(1)?,
                node_type: row.get(2)?,
                source_type: row.get(3)?,
                title: row.get(4)?,
                summary: row.get(5)?,
                keywords_json: row.get(6)?,
                date_from: row.get(7)?,
                date_to: row.get(8)?,
                child_count: row.get(9)?,
                score: row.get(10)?,
            })
        },
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// keywords 未付与の session_log topic ノードを古い順に返す（バックフィル対象）。
pub fn list_topics_missing_keywords(
    conn: &Connection,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS}
         FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'topic' AND source_type = 'session_log'
           AND keywords_json = '[]'
         ORDER BY created_at ASC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![agent_id, limit as i64], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// MEMORY INDEX: カテゴリ層（issue #313）
// ============================================
//
// 時系列ツリー（root→period→session→topic, source_type='session_log'）とは別に、
// 内容で束ねる「カテゴリ index 層」を同じ `memory_index_nodes` に **source_type='category'**
// として被せる。これにより short_id / FTS / browse・search がそのまま効く。
//
// - `node_type='root'` + `source_type='category'`: カテゴリツリーの根（エージェントに 1 つ）。
// - `node_type='category'`: 内容カテゴリ。parent は category-root（畳み込み後は meta）。
// - `node_type='meta'`: カテゴリをさらに束ねたメタ index（段階2で使用）。
//
// topic ↔ category の紐付けは parent 軸ではなく `memory_category_members`（参照表）で持つ
// （topic の session 親を壊さない＝日付から辿る道を残す）。

/// カテゴリツリーの根（`node_type='root'`, `source_type='category'`）を確保して id を返す。
///
/// id はエージェント決定的（`catroot-<agent_id>`）なので `INSERT OR IGNORE` で冪等。
/// 既存の session_log ツリーの根（`root-<agent_id>`）とは id も source_type も別なので
/// 混ざらない。
pub fn ensure_category_root(conn: &Connection, agent_id: &str, now: &str) -> Result<String> {
    let id = format!("catroot-{agent_id}");
    if get_index_node(conn, &id)?.is_some() {
        return Ok(id);
    }
    let short_id = next_short_id(conn, agent_id, "r")?;
    let root = IndexNodeRow {
        id: id.clone(),
        agent_id: agent_id.to_string(),
        parent_id: None,
        node_type: "root".to_string(),
        source_type: "category".to_string(),
        title: "カテゴリ".to_string(),
        summary: String::new(),
        start_log_id: None,
        end_log_id: None,
        source_session_id: None,
        date_from: None,
        date_to: None,
        depth: 0,
        child_count: 0,
        token_count: 0,
        created_at: now.to_string(),
        updated_at: now.to_string(),
        short_id: Some(short_id),
        keywords_json: "[]".to_string(),
        summary_refreshed_at: None,
    };
    insert_index_node(conn, &root)?;
    Ok(id)
}

/// curated 記憶の `long_term/<名前>` から `<名前>` の一覧を返す（カテゴリの種）。
///
/// 読み取り専用。`upsert_curated_memory` の書き込み経路とは競合しない（単一接続で
/// 直列化され、行を書き換えない）。スラッシュを含まない素の `long_term` は対象外
/// （現状データは全て `long_term/<名前>` 形式で、素の行は存在しない）。
pub fn list_long_term_category_seeds(conn: &Connection, agent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT category FROM memory_curated
         WHERE agent_id = ?1 AND category LIKE 'long_term/_%'
         ORDER BY category ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| row.get::<_, String>(0))?;
    let names = rows
        .collect::<std::result::Result<Vec<String>, _>>()?
        .into_iter()
        .filter_map(|c| c.strip_prefix("long_term/").map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    Ok(names)
}

/// カテゴリ層のノード（category / meta）をタイトル完全一致で 1 件引く。
/// 種まき・新規割当の重複作成を防ぐ（同名カテゴリを二重に作らない）。
pub fn get_category_node_by_title(
    conn: &Connection,
    agent_id: &str,
    title: &str,
) -> Result<Option<IndexNodeRow>> {
    let sql = format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND source_type = 'category'
           AND node_type IN ('category','meta') AND title = ?2
         LIMIT 1"
    );
    match conn.query_row(&sql, params![agent_id, title], index_node_from_row) {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// カテゴリノードを 1 件作る（`node_type='category'`, `source_type='category'`）。
/// short_id を採番して返り値に含める。呼び出し側は事前に `ensure_category_root` で
/// 親 id を用意すること。重複作成防止（同名スキップ）は呼び出し側の責務。
///
/// `ensure_category_root` は決定的 id（`catroot-<agent_id>`）を「先に get してから
/// insert」で確保するが、ここは `title` 一致で束ねるべきもので id は決定的にできない
/// （同名カテゴリを別 tick で作れば別 id になる）。そのため同名スキップは id ではなく
/// `get_category_node_by_title` による**タイトル判定**として呼び出し側（core の
/// `assign_unassigned_topics` の resolve）が担う。ここは id を新規計算して
/// `INSERT OR IGNORE` するだけ。id は `cat-<64bit hash>+<nanos>` で、OR IGNORE が
/// 実際に no-op になるのは既存の**別ノード**と id が丸ごと衝突したときだけだが、その
/// 確率は無視できる（hash と nanos が同時一致する必要がある）。衝突時は返り値の id が
/// 実在行を指さないが、呼び出し側が事前にタイトル判定で弾くため到達しない。
pub fn insert_category_node(
    conn: &Connection,
    agent_id: &str,
    parent_id: &str,
    title: &str,
    summary: &str,
    now: &str,
) -> Result<IndexNodeRow> {
    let short_id = next_short_id(conn, agent_id, "c")?;
    let node = IndexNodeRow {
        id: format!("cat-{}", uuid_like(agent_id, title, now)),
        agent_id: agent_id.to_string(),
        parent_id: Some(parent_id.to_string()),
        node_type: "category".to_string(),
        source_type: "category".to_string(),
        title: title.to_string(),
        summary: summary.to_string(),
        start_log_id: None,
        end_log_id: None,
        source_session_id: None,
        date_from: None,
        date_to: None,
        depth: 1,
        child_count: 0,
        token_count: 0,
        created_at: now.to_string(),
        updated_at: now.to_string(),
        short_id: Some(short_id),
        keywords_json: "[]".to_string(),
        summary_refreshed_at: None,
    };
    insert_index_node(conn, &node)?;
    Ok(node)
}

/// id 生成（db クレートは uuid 依存を持たないため、衝突しにくい決定的な id を組む）。
/// agent/title/時刻からハッシュして 16 桁 hex を作る。
fn uuid_like(agent_id: &str, title: &str, now: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    agent_id.hash(&mut h);
    title.hash(&mut h);
    now.hash(&mut h);
    let a = h.finish();
    // 時刻ナノ秒も混ぜて同 tick 内の同名衝突を避ける
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{a:016x}{nanos:08x}")
}

/// カテゴリツリーのトップレベル（category-root 直下の category / meta）を新しい順で返す。
/// 段階3の `Categories:` 注入と、割当プロンプトで「既存カテゴリ一覧」を見せる用途。
pub fn list_top_level_categories(conn: &Connection, agent_id: &str) -> Result<Vec<IndexNodeRow>> {
    let root_id = format!("catroot-{agent_id}");
    let sql = format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND source_type = 'category'
           AND node_type IN ('category','meta') AND parent_id = ?2
         ORDER BY created_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id, root_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// まだどのカテゴリにも割り当てられていない topic を古い順で返す（sleep 中の割当対象）。
/// sticky 割当なので、一度 `memory_category_members` に載った topic は二度と出てこない
/// ＝冪等（同じ入力で同じ結果）。
pub fn list_unassigned_topics(
    conn: &Connection,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    let sql = format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND NOT EXISTS (
               SELECT 1 FROM memory_category_members m
               WHERE m.agent_id = n.agent_id AND m.topic_id = n.id
           )
         ORDER BY n.created_at ASC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id, limit as i64], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// topic にタグ（category ノード）を 1 つ付ける。多対多 PK
/// `(agent_id, topic_id, category_id)`（v26 / #358）なので 1 topic に複数タグを付けられる。
/// 既に同じ 3 つ組が付いていれば `false` を返す（`INSERT OR IGNORE` が PK で弾く＝冪等）。
///
/// **sticky ではない**。v26 で単一ラベル sticky（旧 PK `(agent_id, topic_id)`）はやめ、
/// 付け直し・統合（`merge_tags`）・取り消し（`remove_tag_member`）ができる一期一会の
/// 付与にした（#313）。「未分類」フォールバックは作らない。
pub fn assign_topic_to_category(
    conn: &Connection,
    agent_id: &str,
    topic_id: &str,
    category_id: &str,
    now: &str,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO memory_category_members (agent_id, topic_id, category_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, topic_id, category_id, now],
    )?;
    Ok(n > 0)
}

/// category id → 割当済み topic 数。トップレベル表示の「N 件」やメタ畳み込み判定に使う。
pub fn count_category_members(
    conn: &Connection,
    agent_id: &str,
) -> Result<std::collections::HashMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT category_id, COUNT(*) FROM memory_category_members
         WHERE agent_id = ?1 GROUP BY category_id",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// タグ操作（issue #359 / #313 段階2）
// ============================================
// エージェント自身が記憶（topic）にタグを付ける道具の DB 層。タグは
// `node_type='category'` のノードを流用する（v23 の CHECK に既にある / 新 node_type を
// 足さない）。topic ↔ タグは `memory_category_members`（v26 で多対多 PK）で持つ。
// 呼び出し元（bridge のアクション）は TRUSTED_ONLY で Nostr（caller=Agent）から触らせない。
//
// **sticky にしない**（#313）。付与は取り消せる（`remove_tag_member`）し、統合もできる
// （`merge_tags`）。「未分類」フォールバックは作らない（付かない topic は次の整理で拾う）。

/// タグ名（title 完全一致）で category ノードを解決し、無ければ新設して category_id を返す。
///
/// **タグ新設が黙って失敗しないこと**（#359 / #344 の教訓）。`insert_category_node` は
/// `insert_index_node`（`INSERT OR IGNORE`）経由なので、node_type の CHECK 違反や
/// id / short_id の衝突が握り潰されて `Ok(())` が返る（＝作ったつもりで行が無い）。そのため
/// 新設後に **id で read-back して実在を確認**し、行が無ければ `Err` にする。呼び出し側
/// （アクション）はこれを失敗として報告できる（黙って握り潰さない）。
///
/// 空文字・空白のみのタグ名は拒否する（無名タグを作らない）。既存タグは title 一致で
/// 束ねるので同名を二重に作らない＝冪等。
pub fn resolve_or_create_tag(
    conn: &Connection,
    agent_id: &str,
    tag: &str,
    now: &str,
) -> Result<String> {
    let title = tag.trim();
    if title.is_empty() {
        anyhow::bail!("タグ名が空です");
    }
    // 既存タグ（category / meta ノード）に title 一致があればそれを使う（二重作成しない）。
    if let Some(node) = get_category_node_by_title(conn, agent_id, title)? {
        return Ok(node.id);
    }
    let root_id = ensure_category_root(conn, agent_id, now)?;
    let node = insert_category_node(conn, agent_id, &root_id, title, "", now)?;
    // read-back: OR IGNORE が CHECK 違反 / id・short_id 衝突を握り潰していないか確認する。
    // 実在しなければタグ新設は失敗している（#359: 黙って握り潰さない）。
    if get_index_node(conn, &node.id)?.is_none() {
        anyhow::bail!("タグ「{title}」の新設に失敗しました（ノードが作成されませんでした）");
    }
    Ok(node.id)
}

/// topic に複数タグを付ける（多対多）。無いタグ名は `resolve_or_create_tag` で同時に新設。
///
/// 付与は `memory_category_members` への行追加（`assign_topic_to_category` = `INSERT OR
/// IGNORE`）。同じ `(agent_id, topic_id, category_id)` の二重付与は PK で弾かれる＝冪等。
/// 1 topic に複数の関心があるので複数タグを付けられる。
///
/// タグ新設が失敗したら（`resolve_or_create_tag` が `Err`）その時点で `Err` を返す
/// （黙って握り潰さない / #359）。既に付与済みの分は残る（部分適用は呼び出し側が扱う）。
/// 空白のみの重複タグは title 正規化＋PK 冪等で自然に畳まれる。
pub fn tag_topic(
    conn: &Connection,
    agent_id: &str,
    topic_id: &str,
    tags: &[String],
    now: &str,
) -> Result<()> {
    for tag in tags {
        let category_id = resolve_or_create_tag(conn, agent_id, tag, now)?;
        assign_topic_to_category(conn, agent_id, topic_id, &category_id, now)?;
    }
    Ok(())
}

/// topic からタグ 1 個の付与を取り消す（member 行の削除）。付いていなければ `false`。
///
/// タグノード自体は消さない（他の topic にまだ付いているかもしれない / このアクションは
/// 「この topic からこのタグを外す」だけ）。タグ名は title 一致で category_id へ解決する。
/// 未知のタグ名は `false`（何も外さない）。
pub fn remove_tag_member(
    conn: &Connection,
    agent_id: &str,
    topic_id: &str,
    tag: &str,
) -> Result<bool> {
    let title = tag.trim();
    let Some(node) = get_category_node_by_title(conn, agent_id, title)? else {
        return Ok(false);
    };
    let n = conn.execute(
        "DELETE FROM memory_category_members
         WHERE agent_id = ?1 AND topic_id = ?2 AND category_id = ?3",
        params![agent_id, topic_id, node.id],
    )?;
    Ok(n > 0)
}

/// 統合結果（何件の member を付け替えたか）。呼び出し側の報告用。
#[derive(Debug, Clone)]
pub struct MergeTagsOutcome {
    pub from_category_id: String,
    pub into_category_id: String,
    /// 付け替えた member 行数（付け替え先に既にあった分は数えない）。
    pub moved: usize,
}

/// タグ統合（語彙の整理）。`from` タグの member を全て `into` タグへ付け替え、`from`
/// ノードを削除する。`into` が無ければ新設する（＝実質リネームにもなる）。
///
/// **付け替え先に同じ行が既にある場合に落ちないこと**（#359）: ある topic が `from` と
/// `into` の両方に付いていると、素の `UPDATE ... SET category_id=into` は PK 衝突で落ちる。
/// そこで `INSERT OR IGNORE`（into 行を作る / 既存はスキップ）→ `from` 行を全削除、の順で
/// 行を移す。孤児 member を残さない。
///
/// **タグノード削除で FTS 孤児を残さないこと**（#360 の教訓）: `from` ノードの削除は
/// `delete_index_node`（subtree 再帰 CTE で FTS も同期削除）で行う。生 SQL の DELETE は
/// 使わない。
///
/// `from` と `into` が同一タグに解決される（同名 / 同 id）場合は `Err`（自己統合を拒否 —
/// from ノードを消すと into も消える）。`from` が存在しなければ `Err`。
pub fn merge_tags(
    conn: &Connection,
    agent_id: &str,
    from_tag: &str,
    into_tag: &str,
    now: &str,
) -> Result<MergeTagsOutcome> {
    let from_title = from_tag.trim();
    let into_title = into_tag.trim();
    if from_title.is_empty() || into_title.is_empty() {
        anyhow::bail!("統合元 / 統合先のタグ名が空です");
    }
    let Some(from_node) = get_category_node_by_title(conn, agent_id, from_title)? else {
        anyhow::bail!("統合元タグ「{from_title}」が存在しません");
    };
    // into は無ければ新設（リネームを兼ねる）。新設は read-back で検証済み（黙って失敗しない）。
    let into_id = resolve_or_create_tag(conn, agent_id, into_title, now)?;
    if from_node.id == into_id {
        anyhow::bail!("統合元と統合先が同じタグです（「{from_title}」）");
    }
    with_index_savepoint(conn, |tx| {
        // (1) from の member を into へ複製（付け替え先に既にある行は OR IGNORE でスキップ）。
        let moved = tx.execute(
            "INSERT OR IGNORE INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             SELECT agent_id, topic_id, ?3, created_at
             FROM memory_category_members
             WHERE agent_id = ?1 AND category_id = ?2",
            params![agent_id, from_node.id, into_id],
        )?;
        // (2) from の member を全削除（孤児を残さない）。
        tx.execute(
            "DELETE FROM memory_category_members WHERE agent_id = ?1 AND category_id = ?2",
            params![agent_id, from_node.id],
        )?;
        // (3) from ノードを FTS ごと削除（`delete_index_node` が subtree CTE で FTS も消す）。
        delete_index_node(tx, &from_node.id)?;
        Ok(MergeTagsOutcome {
            from_category_id: from_node.id.clone(),
            into_category_id: into_id.clone(),
            moved: moved as usize,
        })
    })
}
