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
    let inserted = conn.execute(
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
            conn,
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
}

/// ノードを1件削除する（FTS 影テーブルも同期削除）。
///
/// memory_index_nodes への生 SQL DELETE は FTS 孤児を残すため禁止 —
/// 必ずこの関数（または `delete_index_nodes_for_agent`）を使うこと。
pub fn delete_index_node(conn: &Connection, node_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_nodes WHERE id = ?1",
        params![node_id],
    )?;
    conn.execute(
        "DELETE FROM memory_index_fts WHERE node_id = ?1",
        params![node_id],
    )?;
    Ok(())
}

/// ノードの keywords_json を更新する（FTS 同期込み）。
pub fn update_index_node_keywords(
    conn: &Connection,
    node_id: &str,
    keywords_json: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE memory_index_nodes SET keywords_json = ?1, updated_at = ?2 WHERE id = ?3",
        params![keywords_json, Utc::now().to_rfc3339(), node_id],
    )?;
    if let Some(node) = get_index_node(conn, node_id)? {
        fts_upsert_node(
            conn,
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
    conn.execute(
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
    if let Some(current) = get_index_node(conn, &node.id)? {
        fts_upsert_node(
            conn,
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
pub fn delete_index_nodes_for_agent_by_source(
    conn: &Connection,
    agent_id: &str,
    source_type: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_nodes WHERE agent_id = ?1 AND source_type = ?2",
        params![agent_id, source_type],
    )?;
    conn.execute(
        "DELETE FROM memory_index_fts WHERE agent_id = ?1 AND source_type = ?2",
        params![agent_id, source_type],
    )?;
    Ok(())
}

/// エージェントの全インデックスノードを削除する（FTS 影テーブルも同期削除）
pub fn delete_index_nodes_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_nodes WHERE agent_id = ?1",
        params![agent_id],
    )?;
    conn.execute(
        "DELETE FROM memory_index_fts WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
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
    conn.execute(
        "UPDATE memory_index_nodes SET title = ?1, summary = ?2, updated_at = ?3 WHERE id = ?4",
        params![title, summary, Utc::now().to_rfc3339(), node_id],
    )?;
    if let Some(node) = get_index_node(conn, node_id)? {
        fts_upsert_node(
            conn,
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
