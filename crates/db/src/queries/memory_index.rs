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

/// 指定月（date_from が `YYYY-MM` 始まり）の topic のうち、**宣言ユニットが覆って
/// いないもの**を新しい順に返す（#403）。
///
/// 除外は 2 段:
/// - `exclude_session_id` のセッション由来 topic（現セッションの topic は
///   コンパクション時の [Past context summary] が担当 — short_id の重複を避ける）。
///   source_session_id が NULL の topic（merge_topics 産）は含める。
/// - 生ログ id 範囲が宣言ユニット（`node_type='unit'`）と重なる topic。topic は
///   機械の切り方、unit は本人の切り方なので、同じ期間を二重に見せない
///   （[Memory Index] は覆われている範囲を unit 行で見せる）。重なり判定は
///   `u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id`。
///   **id 範囲を持たない（NULL の）topic は落とさない** — 判定できないものを消すと
///   材料が失われる側に倒れるため、比較が NULL になるケースは残す。宣言ユニットが
///   0 件のエージェントでは NOT EXISTS が常に真になり、結果は従来と同一。
///
/// 宣言に飛びがあっても（読んだが宣言しなかった範囲）その範囲の topic は覆われて
/// いないので残る。「宣言カーソルより古いものは一律隠す」ではないのは、飛びの
/// ぶんの記憶を消さないため。
pub fn list_undeclared_topic_nodes_for_month(
    conn: &Connection,
    agent_id: &str,
    month_prefix: &str,
    exclude_session_id: &str,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    // #410: 相関 NOT EXISTS が O(topic 数 × unit 数) で毎ターン走っていた
    // （start_log_id / end_log_id はどの索引にも無いので、生き残る topic では unit 行を
    // 全件舐める）。ここで**先に unit の最大 end_log_id を 1 回だけ**引く。
    //
    // agent_id を `?1` に固定して**非相関**にしているので、SQLite はこのスカラサブクエリを
    // 各 topic ごとではなく **1 度だけ**評価する。topic の start_log_id がその最大値より
    // 大きければ、定義上どの unit の範囲とも重ならない（overlap には
    // `u.end_log_id >= t.start_log_id` が必要だが、全 unit の end_log_id が最大値以下なので
    // 成立しない）＝**必ず未宣言**なので、OR の左辺が真になり相関サブクエリは**評価されない**
    // （SQLite は OR を短絡する）。宣言カーソルより新しい topic は必ずこの条件に入るため、
    // 実運用ではほとんどの行がここで早期確定し、相関サブクエリは古い領域の topic にしか
    // 走らない。
    //
    // 向き: この変更は**走査量を減らす方向にのみ**働く。左辺（`start_log_id > MAX(end)`）が
    // 真の行は NOT EXISTS も必ず真になる（含意関係）ので、返す集合は旧クエリと**不変**。
    // これは下の `rewrite_is_equivalent_to_naive_not_exists` で旧 NOT EXISTS 版と全ケース
    // 同値であることを固定している。
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes t
         WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
           AND t.date_from LIKE ?2 || '%'
           AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
           AND (
             t.start_log_id > (
               SELECT MAX(u.end_log_id) FROM memory_index_nodes u
               WHERE u.agent_id = ?1 AND u.node_type = 'unit'
             )
             OR NOT EXISTS (
               SELECT 1 FROM memory_index_nodes u
               WHERE u.agent_id = ?1 AND u.node_type = 'unit'
                 AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id
             )
           )
         ORDER BY t.created_at DESC LIMIT ?4"
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
    // #425: 未索引統計からエコー行を除外（get_unindexed_log_count と同じ理由）。
    Ok(conn.query_row(
        &format!(
            "SELECT COUNT(*), MAX(created_at) FROM memory_sessions
         WHERE agent_id = ?1 AND id > ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
        ),
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

/// スリープ整理ラン（#313 段階3）のカーソルを取得する。行が無い/NULL なら `None`。
///
/// `last_skill_consolidation_at` と同じ TEXT 1 列だが、中身は
/// **`"{created_at}|{id}"` の複合カーソル**（呼び出し側 `memory_organize` が組み立てる）。
/// 整理ランはこれを 2 つの用途に使う: (1) 日次ゲート（`created_at` 部分を刻時として
/// `now - T >= 間隔`）、(2) bounded worklist の下端（[`list_organize_topics`] の
/// `(created_at, id)` カーソル）。初回シードは `id` 部を持たない素の刻時でよい（`|` が
/// 無ければ全体を `created_at` として解釈する）。`None`（初回遭遇）は呼び出し側が `now` を
/// シードして 1 回スキップする（既存の全 topic を一気に対象化しない）。
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

/// スリープ整理ラン（#313 段階3b / #365）の**遡り消化マーカー**を取得する。
/// 行が無い/NULL なら `None`。
///
/// `last_organize_at`（新規側 / 前進 / 昇順）とは**別軸**の、過去分の遡り消化の進捗。
/// 中身は `last_organize_at` と同形の**複合カーソル `"{created_at}|{id}"`**（呼び出し側
/// `memory_organize` が組み立てる）だが、進む向きが逆で、有効化時の境界（`now`）から
/// **古い方向（降順）**へ「どこまで遡ったか」を刻む。[`list_organize_backlog_topics`] の
/// 上端（＝この位置より古いものが残りの遡り対象）として使う。`None`（未シード）は
/// 呼び出し側が初回遭遇時に `now` をシードする（既存 topic を一気に対象化しない）。
pub fn get_organize_backlog_cursor(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT organize_backlog_cursor FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// スリープ整理ランの遡り消化マーカーを UPSERT で永続化する（行が無ければ作る）。
/// config 行は自動生成されないため、初回シード/遡り前進後にこれで明示的に刻む。
/// 隣の列（`last_organize_at` / `last_skill_consolidation_at`）は触らない。
pub fn set_organize_backlog_cursor(conn: &Connection, agent_id: &str, cursor: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, organize_backlog_cursor)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             organize_backlog_cursor = excluded.organize_backlog_cursor",
        params![
            agent_id,
            BATCH_SIZE_DEFAULT,
            THRESHOLD_DEFAULT,
            chrono::Utc::now().to_rfc3339(),
            cursor,
        ],
    )?;
    Ok(())
}

/// スリープ整理ラン（#313 段階3b / #365）の**日次 throttle 用の最終実行刻時**を取得する。
/// 行が無い/NULL なら `None`。
///
/// 2 軸の位置マーカー（`last_organize_at` / `organize_backlog_cursor`）とは別で、これは
/// **壁時計の刻時**。整理ランは clean 完了ごとにこれを `now` へ進め、日次ゲート
/// （`now - organize_last_run_at >= 間隔`）の基準にする。位置マーカーを壁時計へ飛ばすと、
/// 非トランザクションなビルドが途中失敗して `end_log_id > watermark`（snapshot 外）の
/// topic を残したとき、その topic を新規側カーソルが追い越して恒久ロスするため、時刻と
/// 位置を分離する（#365 レビュー修正 / #364 blocker と同型の取りこぼし回避）。
pub fn get_organize_last_run_at(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT organize_last_run_at FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 整理ランの最終実行刻時（throttle）を UPSERT で永続化する（行が無ければ作る）。
/// 隣の列（2 軸の位置マーカー・skill 棚卸し）は触らない。
pub fn set_organize_last_run_at(conn: &Connection, agent_id: &str, ts: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, organize_last_run_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             organize_last_run_at = excluded.organize_last_run_at",
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

/// 宣言ラン（#384 / #376 段階2）の**進捗マーカー**を取得する。行が無い/NULL なら `None`。
///
/// タグ整理ランの 3 列（`last_organize_at` / `organize_backlog_cursor` /
/// `organize_last_run_at`）とは別の**単一マーカー**。中身は複合カーソル
/// **`"{last_run_at_rfc3339}|{cursor_log_id}"`**（呼び出し側 `memory_declare` が組み立てる）:
/// 左が日次 throttle 用の壁時計、右が生ログ id 上の昇順・前進のみの位置（提示し終えた末尾）。
/// 生ログは不変・append-only・id 単調増加なので、位置を id で持てば snapshot/watermark に
/// 依存せず追い越しの罠（#365）を避けられ、throttle と位置を 1 列で両立できる。`None`
/// （未実行）は呼び出し側が `(throttle 無し, cursor=0)` と解釈し、生ログの先頭から始める。
pub fn get_memory_declare_cursor(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT memory_declare_cursor FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 宣言ランの進捗マーカーを UPSERT で永続化する（行が無ければ作る）。
/// 隣の列（タグ整理ランの 3 マーカー・skill 棚卸し）は触らない。
pub fn set_memory_declare_cursor(conn: &Connection, agent_id: &str, cursor: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, memory_declare_cursor)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             memory_declare_cursor = excluded.memory_declare_cursor",
        params![
            agent_id,
            BATCH_SIZE_DEFAULT,
            THRESHOLD_DEFAULT,
            chrono::Utc::now().to_rfc3339(),
            cursor,
        ],
    )?;
    Ok(())
}

/// 宣言ラン（#394）の**窓の希望**。本人が `plan_next_memory_window` で表明した内容。
///
/// 窓の境界と広さを機械が固定で決めていた（カーソルは宣言内容と無関係に窓の終端へ進む）のを、
/// 「どこからどこまでが 1 つの記憶かは本人が決める」という宣言ランの設計に揃えるための箱。
/// **希望であって決定ではない**: ランの側が前進の下限・上限へ丸めてから使う（本人任せにすると
/// 同じ窓を永久に再取得するループに入る / #374）。
///
/// フィールドは**寿命が違う**:
/// - `next_from_id` と `note` はそのランの終わりに消費されて消える（持ち越さない）。過去の
///   指定が後のランのカーソルを勝手に引き戻さないため。`note` は「その位置をそう決めた理由」
///   なので寿命は位置と同じ（残すと以後すべてのランの監査に同じ文字列が出続け、そのランで
///   書かれたものと誤読される）。
/// - `window_size` は sticky（本人が上書きするまで効き続ける）。「今回は薄かったので次から
///   もっと広く」という調整は、1 回きりではなく本人の設定として残るのが自然だから。
/// - `partial_streak` だけは**機械が持つ状態**（本人は書かない）。広さを sticky にした結果、
///   広げすぎてターンが毎回潰れる状態から自力で戻れなくなるのを防ぐためのカウンタ。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeclareWindowPref {
    /// 次回の窓をこの生ログ id から始めたい（＝この id 以降は未処理として次回へ回す）。
    /// ランが消費したら `None` に戻る。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_from_id: Option<i64>,
    /// 次回以降の窓に入れたい生ログ件数（sticky）。`None` なら config の既定を使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_size: Option<i64>,
    /// 本人が書いた理由。監査ログに載せるだけで、機械は解釈しない。位置と一緒に消費される。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// 最後に書いた時刻（RFC3339）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// `window_size` を表明した状態で partial が**連続**した回数（機械が刻む / 本人は書かない）。
    /// clean が 1 回通れば `None` に戻る。既定値へ戻したときも `None` に戻る。丸めの規則と
    /// 上限は `crates/server/src/memory_declare.rs` にある。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_streak: Option<i64>,
}

/// 宣言ランの窓の希望（#394）を取得する。行が無い / NULL / 壊れた JSON なら `None`。
///
/// 壊れた JSON でエラーにしないのは、この列が**任意の希望**でしかないため。読めなければ
/// 「希望なし」として従来どおり（窓の終端まで前進 / config の広さ）に倒れるのが安全側。
pub fn get_memory_declare_window(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<DeclareWindowPref>> {
    let raw = conn.query_row(
        "SELECT memory_declare_window FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    let raw = match raw {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };
    Ok(raw.and_then(|s| serde_json::from_str::<DeclareWindowPref>(&s).ok()))
}

/// 宣言ランの窓の希望を UPSERT で永続化する（行が無ければ作る）。
/// `None` を渡すと列を NULL に戻す（希望なし）。隣の列（マーカー等）は触らない。
pub fn set_memory_declare_window(
    conn: &Connection,
    agent_id: &str,
    pref: Option<&DeclareWindowPref>,
) -> Result<()> {
    let raw = match pref {
        Some(p) => Some(serde_json::to_string(p)?),
        None => None,
    };
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, memory_declare_window)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             memory_declare_window = excluded.memory_declare_window",
        params![
            agent_id,
            BATCH_SIZE_DEFAULT,
            THRESHOLD_DEFAULT,
            chrono::Utc::now().to_rfc3339(),
            raw,
        ],
    )?;
    Ok(())
}

/// スリープ整理ラン（#313 段階3）の worklist 対象 topic 数を数える（発火の下限ゲート用）。
///
/// 対象 = `node_type='topic'` かつ `source_type='session_log'` で、
/// (a) 前回カーソル `since = (created_at, id)` より**後**、(b) スナップショット
/// `snapshot_log_id`（`memory_index_watermark.last_indexed_log_id`）以下に収まっているもの。
/// `since=None` なら下端制約なし。`end_log_id IS NULL` の topic はスナップショット内とみなす。
///
/// **カーソルは `created_at` 単体でなく `(created_at, id)` の単調タプル**にしている。索引
/// ビルドは 1 パスの全 topic に**同一 `created_at`** を刻む（`index_builder.rs`）ため、
/// `created_at` 単体で `> T` すると、切り口が同着群の内側に落ちたとき同じ `created_at` を持つ
/// 未提示分が二度と対象にならず取りこぼす。`id` を副キーにして境界を跨いで残余を引き継ぐ。
pub fn count_organize_topics(
    conn: &Connection,
    agent_id: &str,
    since: Option<(&str, &str)>,
    snapshot_log_id: i64,
) -> Result<i64> {
    let (since_ts, since_id) = match since {
        Some((ts, id)) => (Some(ts), id),
        None => (None, ""),
    };
    let n = conn.query_row(
        "SELECT COUNT(*) FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND (?2 IS NULL OR n.created_at > ?2 OR (n.created_at = ?2 AND n.id > ?3))
           AND (n.end_log_id IS NULL OR n.end_log_id <= ?4)",
        params![agent_id, since_ts, since_id, snapshot_log_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(n)
}

/// スリープ整理ランの worklist（対象 topic を `(created_at, id)` 昇順で最大 `limit` 件）を返す。
///
/// フィルタは [`count_organize_topics`] と同一の `(created_at, id)` カーソル。並び順も
/// `created_at ASC, id ASC` で揃えてあるので、`limit` で切った残り（＝カーソルより後）は、
/// 呼び出し側が**末尾の `(created_at, id)` をマーカーへ刻めば**次回そこから引き継げる
/// （前進のみ / 残りは次回 / 同着 created_at 群を N で分断しても取りこぼさない）。
pub fn list_organize_topics(
    conn: &Connection,
    agent_id: &str,
    since: Option<(&str, &str)>,
    snapshot_log_id: i64,
    limit: i64,
) -> Result<Vec<IndexNodeRow>> {
    let (since_ts, since_id) = match since {
        Some((ts, id)) => (Some(ts), id),
        None => (None, ""),
    };
    let sql = format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND (?2 IS NULL OR n.created_at > ?2 OR (n.created_at = ?2 AND n.id > ?3))
           AND (n.end_log_id IS NULL OR n.end_log_id <= ?4)
         ORDER BY n.created_at ASC, n.id ASC LIMIT ?5"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![agent_id, since_ts, since_id, snapshot_log_id, limit],
        index_node_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// スリープ整理ラン（#313 段階3b / #365）の**遡り消化**の残数を数える（監査・先頭到達判定用）。
///
/// 対象 = `node_type='topic'` かつ `source_type='session_log'` で、遡りカーソル
/// `before = (created_at, id)` より**古い方（降順で後ろ）**にあるもの:
/// `created_at < before_ts OR (created_at = before_ts AND id < before_id)`。境界に置いた
/// `now`（有効化時のシード）より古い＝過去分だけが対象になる。スナップショット
/// `snapshot_log_id` 以下に絞るのは新規側 [`count_organize_topics`] と同じ（過去 topic は
/// 全て索引済みなので実質恒真だが、対称性と防御のため残す）。
///
/// **カーソルは `created_at` 単体でなく `(created_at, id)` の単調タプル**。索引ビルドは 1 パスの
/// 全 topic に**同一 `created_at`** を刻むため、`created_at` 単体で `< T` すると同着群を N で
/// 切ったとき残余を恒久的に取りこぼす。遡りは**降順**なので比較は `<`（新規側の `>` と逆向き）。
pub fn count_organize_backlog_topics(
    conn: &Connection,
    agent_id: &str,
    before: (&str, &str),
    snapshot_log_id: i64,
) -> Result<i64> {
    let (before_ts, before_id) = before;
    let n = conn.query_row(
        "SELECT COUNT(*) FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND (n.created_at < ?2 OR (n.created_at = ?2 AND n.id < ?3))
           AND (n.end_log_id IS NULL OR n.end_log_id <= ?4)",
        params![agent_id, before_ts, before_id, snapshot_log_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(n)
}

/// スリープ整理ランの遡り worklist（過去 topic を `(created_at, id)` **降順**で最大 `limit` 件）を返す。
///
/// フィルタは [`count_organize_backlog_topics`] と同一の `(created_at, id)` 遡りカーソル。並び順は
/// `created_at DESC, id DESC` で、`limit` で切った末尾（＝提示した中で**最も古い** `(created_at, id)`）を
/// マーカーへ刻めば、次回はそこより古い分だけが対象になる（前進のみ / 残りは次回 / 同着 created_at 群を
/// N で分断しても取りこぼさない）。先頭（最古）に到達すると 0 件を返して止まる（無限に走らない）。
pub fn list_organize_backlog_topics(
    conn: &Connection,
    agent_id: &str,
    before: (&str, &str),
    snapshot_log_id: i64,
    limit: i64,
) -> Result<Vec<IndexNodeRow>> {
    let (before_ts, before_id) = before;
    let sql = format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes n
         WHERE n.agent_id = ?1 AND n.node_type = 'topic' AND n.source_type = 'session_log'
           AND (n.created_at < ?2 OR (n.created_at = ?2 AND n.id < ?3))
           AND (n.end_log_id IS NULL OR n.end_log_id <= ?4)
         ORDER BY n.created_at DESC, n.id DESC LIMIT ?5"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![agent_id, before_ts, before_id, snapshot_log_id, limit],
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

// ============================================
// 記憶の単位（宣言ノード / issue #379 #376 段階1）
// ============================================
//
// エージェントが自分の生ログの範囲 `[from_id, to_id]` を「1 つの記憶」として宣言する。
// 宣言は time-series ツリー（root→period→session→topic, `source_type='session_log'`）や
// カテゴリ層（`source_type='category'`）とは**別 `node_type='unit'`** として載せる
// （`source_type='declared'` も併記）。`node_type='unit'` は既存の全 `node_type='topic'`
// 述語（rollup / タグ整理 worklist / `get_topic_nodes_for_session` / `merge_topics` 等）から
// 構造的に外れるので、二重要約・二重タグ付けは起きない（#379 監査で確定 / v30 で CHECK 拡張）。
//
// 親は専用ルート `declroot-<agent_id>`（`node_type='root'`, `source_type='declared'`,
// parent_id=NULL）。short_id は `u` 系列。**生ログ（memory_sessions）は消さない・変えない**
// ので、宣言は何度でも取り消して付け直せる（`retract_memory_unit`）。

/// 宣言ツリーの根（`node_type='root'`, `source_type='declared'`）を確保して id を返す。
///
/// id はエージェント決定的（`declroot-<agent_id>`）なので「先に get → 無ければ insert」で
/// 冪等（`ensure_category_root` と同型）。session_log ツリーの根（`root-<agent_id>`）とも
/// カテゴリ根（`catroot-<agent_id>`）とも id・source_type が別なので混ざらない。
pub fn ensure_declared_root(conn: &Connection, agent_id: &str, now: &str) -> Result<String> {
    let id = format!("declroot-{agent_id}");
    if get_index_node(conn, &id)?.is_some() {
        return Ok(id);
    }
    let short_id = next_short_id(conn, agent_id, "r")?;
    let root = IndexNodeRow {
        id: id.clone(),
        agent_id: agent_id.to_string(),
        parent_id: None,
        node_type: "root".to_string(),
        source_type: "declared".to_string(),
        title: "宣言した記憶".to_string(),
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
    // read-back: OR IGNORE / CHECK 違反で黙って握り潰されていないか確認（#344）。
    if get_index_node(conn, &id)?.is_none() {
        anyhow::bail!("宣言ルートの作成に失敗しました（ノードが作成されませんでした）");
    }
    Ok(id)
}

/// 生ログの範囲 `[from_id, to_id]` を 1 つの記憶として宣言する（`node_type='unit'`）。
///
/// `title` 必須・`summary` 任意。`source_session_id` は必ず NULL（宣言はセッションに
/// 紐付けない — `get_topic_nodes_for_session` 等の `source_session_id` 絞りから外れる）。
/// **重なりは禁止しない**（1 つの範囲が複数ユニットに属してよい / 既存ユニットとの
/// start/end 重複はチェックしない）。作成後に **id で read-back** して実在を確認する
/// （`INSERT OR IGNORE` がノードを黙って握り潰していないか / #344）。生ログには触らない。
#[allow(clippy::too_many_arguments)]
pub fn record_memory_unit(
    conn: &Connection,
    agent_id: &str,
    title: &str,
    summary: &str,
    from_id: i64,
    to_id: i64,
    date_from: Option<&str>,
    date_to: Option<&str>,
    now: &str,
) -> Result<IndexNodeRow> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("title が空です");
    }
    let root_id = ensure_declared_root(conn, agent_id, now)?;
    let short_id = next_short_id(conn, agent_id, "u")?;
    let node = IndexNodeRow {
        id: format!("unit-{}", uuid_like(agent_id, title, now)),
        agent_id: agent_id.to_string(),
        parent_id: Some(root_id),
        node_type: "unit".to_string(),
        source_type: "declared".to_string(),
        title: title.to_string(),
        summary: summary.to_string(),
        start_log_id: Some(from_id),
        end_log_id: Some(to_id),
        source_session_id: None,
        date_from: date_from.map(str::to_string),
        date_to: date_to.map(str::to_string),
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
    // read-back: 実在しなければ宣言は失敗している（#344: 黙って握り潰さない）。
    if get_index_node(conn, &node.id)?.is_none() {
        anyhow::bail!("記憶の宣言に失敗しました（ノードが作成されませんでした）");
    }
    Ok(node)
}

/// 宣言ユニットを取り消す（**宣言ノード + FTS 行 + member 行だけ**を消す。生ログは不変）。
///
/// `unit_ref` は short_id またはフル id。**`node_type='unit'` のノードだけ**を対象にする
/// （session_log topic やカテゴリノードを誤って消さないための安全ガード）。ユニットに
/// 付いたタグの member 行（`memory_category_members.topic_id = unit.id`）も消す＝原状復帰。
/// FTS は `delete_index_node`（subtree CTE で FTS も消す）に委ねて孤児を残さない（v26 の罠）。
///
/// 戻り値: 取り消したユニットのフル id。見つからない / ユニットでない場合は `Err`。
pub fn retract_memory_unit(conn: &Connection, agent_id: &str, unit_ref: &str) -> Result<String> {
    let node = match get_index_node_by_short_or_id(conn, agent_id, unit_ref)? {
        Some(n) => n,
        None => anyhow::bail!("宣言ユニット「{unit_ref}」が見つかりません"),
    };
    if node.node_type != "unit" {
        anyhow::bail!(
            "「{unit_ref}」は宣言ユニット（node_type='unit'）ではありません（node_type='{}'）。retract は宣言ユニットのみ取り消せます",
            node.node_type
        );
    }
    with_index_savepoint(conn, |tx| {
        // (1) ユニットに付いたタグの member 行を消す（孤児を残さない / 原状復帰）。
        tx.execute(
            "DELETE FROM memory_category_members WHERE agent_id = ?1 AND topic_id = ?2",
            params![agent_id, node.id],
        )?;
        // (2) 宣言ノード本体を FTS ごと削除（unit は葉なので subtree は自分だけ）。
        delete_index_node(tx, &node.id)?;
        Ok(())
    })?;
    Ok(node.id)
}

/// エージェントの宣言ユニット一覧（新しい順）。survey / テスト・監査で使う。
pub fn list_memory_units(conn: &Connection, agent_id: &str) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'unit'
         ORDER BY start_log_id DESC, created_at DESC"
    ))?;
    let rows = stmt.query_map(params![agent_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 宣言ユニットの新しい方から `limit` 件（#403 の [Memory Index] 注入用）。
/// 並びは [`list_memory_units`] と同じ（生ログ位置の新しい順）。全件版と分けるのは、
/// 会話のたびに走る注入で 200 件超の summary まで読み込まないため。
pub fn list_recent_memory_units(
    conn: &Connection,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'unit'
         ORDER BY start_log_id DESC, created_at DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![agent_id, limit as i64], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 宣言ユニットの総数（[Memory Index] の「…and N older」畳み行用、凝縮ランのゲート用）。
pub fn count_memory_units(conn: &Connection, agent_id: &str) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_index_nodes WHERE agent_id = ?1 AND node_type = 'unit'",
        params![agent_id],
        |row| row.get(0),
    )?;
    Ok(n as usize)
}

/// 凝縮の**逐次窓**（#411 / オーナー指摘: 蓄積分を一括で与えない）。カーソル位置
/// `after_start_log_id` より新しいユニットを**時系列（古い→新しい）順に最大 `limit` 件**返す。
/// 凝縮ランは毎回この窓（＋既存 core 全件）だけを見て、更新優先で core を育てる。一括で全
/// ユニットを渡すと平均化に寄るため、新規エージェントが少しずつ積むのと同じ窓幅で消化する。
pub fn list_memory_units_after(
    conn: &Connection,
    agent_id: &str,
    after_start_log_id: i64,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'unit' AND start_log_id > ?2
         ORDER BY start_log_id ASC, created_at ASC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(
        params![agent_id, after_start_log_id, limit as i64],
        index_node_from_row,
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// カーソル位置 `after_start_log_id` より新しい（＝まだ凝縮していない）ユニットの残数。
/// 発火判定（残 >= 窓幅なら積み残し消化、0 < 残 < 窓幅なら min_interval を待って末尾を流す）に使う。
pub fn count_memory_units_after(
    conn: &Connection,
    agent_id: &str,
    after_start_log_id: i64,
) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'unit' AND start_log_id > ?2",
        params![agent_id, after_start_log_id],
        |row| row.get(0),
    )?;
    Ok(n as usize)
}

// ============================================
// MEMORY: 凝縮（記憶の 3 段目 / issue #411）
// ============================================
//
// 凝縮は「その出来事たちが何を意味するか」を本人が抽出したもの（`node_type='meta'`）。
// 材料は生ログではなく**自分のユニット**（宣言物）で、原則 1 件 = {軸ラベル(title) + 本文(summary)
// + 根拠ユニットへのリンク}。根拠リンクは keywords_json に**元ユニットの short_id の JSON 配列**
// として持ち、id 範囲（start_log_id / end_log_id）は元ユニットの生ログ範囲の min / max を粗く畳む。
// **具体を失った凝縮は平均化**（#411 の原則3）なので、根拠リンクを構造として必須にする
// （record は最低 1 件の元ユニットを解決できないと失敗する）。
//
// ユニット（`node_type='unit'`）とは置き場も注入面も分ける（ユニット=索引 / 凝縮=人格の核）。
// エージェント間で混ぜない（全クエリが agent_id 固定）。生ログには一切触らない。

/// 凝縮ノードをぶら下げるルート（`node_type='root'` / `source_type='condensed'`）を確保する。
/// 宣言ルート（[`ensure_declared_root`]）とは別のルートにして、俯瞰時に索引（ユニット）と
/// 核（凝縮）を取り違えないようにする。
pub fn ensure_condensed_root(conn: &Connection, agent_id: &str, now: &str) -> Result<String> {
    let id = format!("condroot-{agent_id}");
    if get_index_node(conn, &id)?.is_some() {
        return Ok(id);
    }
    let short_id = next_short_id(conn, agent_id, "r")?;
    let root = IndexNodeRow {
        id: id.clone(),
        agent_id: agent_id.to_string(),
        parent_id: None,
        node_type: "root".to_string(),
        source_type: "condensed".to_string(),
        title: "凝縮した記憶".to_string(),
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
    if get_index_node(conn, &id)?.is_none() {
        anyhow::bail!("凝縮ルートの作成に失敗しました（ノードが作成されませんでした）");
    }
    Ok(id)
}

/// 元ユニット参照（short_id またはフル id）の並びを解決する。
///
/// 各参照が**このエージェントの `node_type='unit'`** を指すことを確認し、正規化した
/// short_id（フル id 指定でも short_id へ寄せる）と、生ログ範囲の min(start) / max(end) を返す。
/// 解決できなかった参照は `unresolved` に積む（呼び出し側で報告する）。
struct ResolvedSources {
    /// 解決できた元ユニットの short_id（重複除去・入力順）。keywords_json に載せる。
    short_ids: Vec<String>,
    /// 元ユニット群の生ログ範囲の下端（解決 0 件なら None）。
    min_start: Option<i64>,
    /// 元ユニット群の生ログ範囲の上端（解決 0 件なら None）。
    max_end: Option<i64>,
    /// 解決できなかった参照（そのまま返す）。
    unresolved: Vec<String>,
}

fn resolve_unit_sources(
    conn: &Connection,
    agent_id: &str,
    refs: &[String],
) -> Result<ResolvedSources> {
    let mut short_ids: Vec<String> = Vec::new();
    let mut min_start: Option<i64> = None;
    let mut max_end: Option<i64> = None;
    let mut unresolved: Vec<String> = Vec::new();
    for r in refs {
        let r = r.trim();
        if r.is_empty() {
            continue;
        }
        match get_index_node_by_short_or_id(conn, agent_id, r)? {
            Some(node) if node.node_type == "unit" => {
                let key = node.short_id.clone().unwrap_or_else(|| node.id.clone());
                if !short_ids.contains(&key) {
                    short_ids.push(key);
                    if let Some(s) = node.start_log_id {
                        min_start = Some(min_start.map_or(s, |m: i64| m.min(s)));
                    }
                    if let Some(e) = node.end_log_id {
                        max_end = Some(max_end.map_or(e, |m: i64| m.max(e)));
                    }
                }
            }
            // ユニット以外 / 見つからない参照は無視して報告する（他ノード種を根拠にしない）。
            _ => unresolved.push(r.to_string()),
        }
    }
    Ok(ResolvedSources {
        short_ids,
        min_start,
        max_end,
        unresolved,
    })
}

/// 凝縮の記録結果（呼び出し側の監査・応答に使う）。
pub struct RecordCoreResult {
    pub node: IndexNodeRow,
    /// 解決できた元ユニットの short_id。
    pub sources: Vec<String>,
    /// 解決できなかった参照（あれば呼び出し側が報告する）。
    pub unresolved: Vec<String>,
}

/// 凝縮（原則）を 1 件記録する（`node_type='meta'` / `source_type='condensed'`）。
///
/// `axis`（軸ラベル = title）必須・`body`（本文 = summary）必須・`sources`（根拠ユニットの
/// short_id）**最低 1 件が解決できること**が必須（根拠 0 件は平均化なので受け付けない / #411 原則3）。
/// 根拠は keywords_json に short_id の JSON 配列で持ち、id 範囲は元ユニットの min/max を畳む。
/// 作成後に id で read-back して実在を確認する（#344）。
pub fn record_memory_core(
    conn: &Connection,
    agent_id: &str,
    axis: &str,
    body: &str,
    sources: &[String],
    now: &str,
) -> Result<RecordCoreResult> {
    let axis = axis.trim();
    if axis.is_empty() {
        anyhow::bail!("軸ラベル（axis）が空です");
    }
    let resolved = resolve_unit_sources(conn, agent_id, sources)?;
    if resolved.short_ids.is_empty() {
        anyhow::bail!(
            "根拠にできる元ユニットが 1 件も解決できませんでした（sources に自分の宣言ユニットの short_id を指定してください）"
        );
    }
    let root_id = ensure_condensed_root(conn, agent_id, now)?;
    let short_id = next_short_id(conn, agent_id, "m")?;
    let keywords_json = serde_json::to_string(&resolved.short_ids)?;
    let node = IndexNodeRow {
        id: format!("core-{}", uuid_like(agent_id, axis, now)),
        agent_id: agent_id.to_string(),
        parent_id: Some(root_id),
        node_type: "meta".to_string(),
        source_type: "condensed".to_string(),
        title: axis.to_string(),
        summary: body.to_string(),
        start_log_id: resolved.min_start,
        end_log_id: resolved.max_end,
        source_session_id: None,
        date_from: None,
        date_to: None,
        depth: 1,
        child_count: 0,
        token_count: 0,
        created_at: now.to_string(),
        updated_at: now.to_string(),
        short_id: Some(short_id),
        keywords_json,
        summary_refreshed_at: None,
    };
    insert_index_node(conn, &node)?;
    if get_index_node(conn, &node.id)?.is_none() {
        anyhow::bail!("凝縮の記録に失敗しました（ノードが作成されませんでした）");
    }
    Ok(RecordCoreResult {
        node,
        sources: resolved.short_ids,
        unresolved: resolved.unresolved,
    })
}

/// 既存の凝縮（`node_type='meta'`）を更新する（#411 原則4: 新規追加より更新を優先）。
///
/// `core_ref` は short_id またはフル id。**`node_type='meta'` のノードだけ**を対象にする
/// （ユニットや topic を誤って書き換えない安全ガード）。`sources` を渡したときだけ根拠リンクと
/// id 範囲を差し替える（None のときは根拠を維持し、軸・本文だけ更新する）。
pub fn update_memory_core(
    conn: &Connection,
    agent_id: &str,
    core_ref: &str,
    axis: &str,
    body: &str,
    sources: Option<&[String]>,
) -> Result<RecordCoreResult> {
    let axis = axis.trim();
    if axis.is_empty() {
        anyhow::bail!("軸ラベル（axis）が空です");
    }
    let node = match get_index_node_by_short_or_id(conn, agent_id, core_ref)? {
        Some(n) => n,
        None => anyhow::bail!("凝縮「{core_ref}」が見つかりません"),
    };
    // 凝縮ノードだけを対象にする。node_type='meta' は段階2 でタグ整理側が
    // source_type='category' の meta を作りうる（#313）ので、**source_type='condensed' も要求**する
    // （カテゴリ側の meta を凝縮道具で誤って書き換えない構造ガード。3 経路で対称に効かせる）。
    if node.node_type != "meta" || node.source_type != "condensed" {
        anyhow::bail!(
            "「{core_ref}」は凝縮（node_type='meta' / source_type='condensed'）ではありません（node_type='{}' / source_type='{}'）。update_memory_core は凝縮のみ更新できます",
            node.node_type,
            node.source_type
        );
    }

    // sources を渡したときだけ根拠を差し替える。渡した以上は 1 件以上解決できること。
    let (keywords_json, min_start, max_end, resolved_sources, unresolved) = match sources {
        Some(refs) => {
            let resolved = resolve_unit_sources(conn, agent_id, refs)?;
            if resolved.short_ids.is_empty() {
                anyhow::bail!(
                    "根拠にできる元ユニットが 1 件も解決できませんでした（sources を省略すると既存の根拠を維持します）"
                );
            }
            (
                Some(serde_json::to_string(&resolved.short_ids)?),
                resolved.min_start,
                resolved.max_end,
                resolved.short_ids,
                resolved.unresolved,
            )
        }
        None => {
            // 既存の根拠（keywords_json）をそのまま維持。応答用に読み出す。
            let existing: Vec<String> =
                serde_json::from_str(&node.keywords_json).unwrap_or_default();
            (
                None,
                node.start_log_id,
                node.end_log_id,
                existing,
                Vec::new(),
            )
        }
    };

    let now = Utc::now().to_rfc3339();
    with_index_savepoint(conn, |tx| {
        if let Some(kw) = &keywords_json {
            tx.execute(
                "UPDATE memory_index_nodes
                 SET title = ?1, summary = ?2, keywords_json = ?3,
                     start_log_id = ?4, end_log_id = ?5, updated_at = ?6
                 WHERE id = ?7",
                params![axis, body, kw, min_start, max_end, now, node.id],
            )?;
        } else {
            tx.execute(
                "UPDATE memory_index_nodes
                 SET title = ?1, summary = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![axis, body, now, node.id],
            )?;
        }
        // FTS を最新の title / summary / keywords で貼り直す（孤児を残さない）。
        if let Some(fresh) = get_index_node(tx, &node.id)? {
            fts_upsert_node(
                tx,
                &fresh.id,
                &fresh.agent_id,
                &fresh.node_type,
                &fresh.source_type,
                &fresh.title,
                &fresh.summary,
                &fresh.keywords_json,
            )?;
        }
        Ok(())
    })?;

    let fresh = get_index_node(conn, &node.id)?
        .ok_or_else(|| anyhow::anyhow!("凝縮の更新に失敗しました（ノードが消えました）"))?;
    Ok(RecordCoreResult {
        node: fresh,
        sources: resolved_sources,
        unresolved,
    })
}

/// 凝縮を取り消す（**凝縮ノード + FTS 行だけ**を消す。生ログにも元ユニットにも触らない）。
///
/// `core_ref` は short_id またはフル id。**凝縮ノード（node_type='meta' / source_type='condensed'）
/// だけ**を対象にする（カテゴリ側の meta を誤って消さない構造ガード）。
/// 戻り値: 取り消した凝縮のフル id。
pub fn retract_memory_core(conn: &Connection, agent_id: &str, core_ref: &str) -> Result<String> {
    let node = match get_index_node_by_short_or_id(conn, agent_id, core_ref)? {
        Some(n) => n,
        None => anyhow::bail!("凝縮「{core_ref}」が見つかりません"),
    };
    if node.node_type != "meta" || node.source_type != "condensed" {
        anyhow::bail!(
            "「{core_ref}」は凝縮（node_type='meta' / source_type='condensed'）ではありません（node_type='{}' / source_type='{}'）。retract_memory_core は凝縮のみ取り消せます",
            node.node_type,
            node.source_type
        );
    }
    with_index_savepoint(conn, |tx| {
        delete_index_node(tx, &node.id)?;
        Ok(())
    })?;
    Ok(node.id)
}

/// エージェントの凝縮一覧（新しい順）。凝縮ランのプロンプト同梱・監査・テストで使う。
///
/// **`source_type='condensed'` で絞る**（node_type='meta' だけだと、段階2 でタグ整理側が作る
/// source_type='category' の meta 行を凝縮として拾ってしまう / #313 と対称）。
pub fn list_memory_cores(conn: &Connection, agent_id: &str) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS} FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'meta' AND source_type = 'condensed'
         ORDER BY updated_at DESC, created_at DESC"
    ))?;
    let rows = stmt.query_map(params![agent_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 凝縮ラン（#411）の進捗マーカーを読む。形式は複合カーソル `"{last_run_at}|{unit_count}"`
/// （宣言ランの `memory_declare_cursor` と同型。位置部は「前回凝縮した時点のユニット総数」）。
pub fn get_memory_condense_cursor(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT memory_condense_cursor FROM agent_memory_index_config WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 凝縮ランの進捗マーカーを UPSERT で永続化する（行が無ければ作る）。
/// 隣の列（宣言/整理ランのマーカー・skill 棚卸し）は触らない。
pub fn set_memory_condense_cursor(conn: &Connection, agent_id: &str, cursor: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_memory_index_config
             (agent_id, batch_size, threshold, updated_at, memory_condense_cursor)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
             memory_condense_cursor = excluded.memory_condense_cursor",
        params![
            agent_id,
            BATCH_SIZE_DEFAULT,
            THRESHOLD_DEFAULT,
            chrono::Utc::now().to_rfc3339(),
            cursor,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod undeclared_topic_tests {
    //! #410: `list_undeclared_topic_nodes_for_month` の O(topic×unit) 相関 NOT EXISTS を
    //! 「先に unit の MAX(end_log_id) を 1 回引いて OR で短絡する」形に書き換えた。
    //! ここで固定するのは **結果集合が旧クエリと 1 行も変わらないこと**（最適化は
    //! 走査量だけを減らし、返す行は不変）。OR 短絡で分岐が増えたので、左辺で早期確定した
    //! topic（短絡した側）と、左辺が偽で NOT EXISTS まで回った topic（回った側）の両方を通す。
    use super::*;

    const MONTH: &str = "2026-01";

    fn topic(
        id: &str,
        start: Option<i64>,
        end: Option<i64>,
        sess: Option<&str>,
        created: &str,
    ) -> IndexNodeRow {
        IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: id.to_string(),
            summary: "s".to_string(),
            start_log_id: start,
            end_log_id: end,
            source_session_id: sess.map(str::to_string),
            date_from: Some("2026-01-15".to_string()),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: created.to_string(),
            updated_at: created.to_string(),
            short_id: None,
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    fn unit(id: &str, start: Option<i64>, end: Option<i64>) -> IndexNodeRow {
        IndexNodeRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "unit".to_string(),
            source_type: "session_log".to_string(),
            title: id.to_string(),
            summary: "s".to_string(),
            start_log_id: start,
            end_log_id: end,
            source_session_id: None,
            date_from: Some("2026-01-15".to_string()),
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: None,
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        }
    }

    /// 書き換え前の素の相関 NOT EXISTS 版（黄金の実装）。本番関数の結果はこれと
    /// 全ケース一致しなければならない。**このクエリは変更してはいけない**（比較基準）。
    fn naive_undeclared(
        conn: &Connection,
        agent_id: &str,
        month_prefix: &str,
        exclude_session_id: &str,
        limit: usize,
    ) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT t.id FROM memory_index_nodes t
                 WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
                   AND t.date_from LIKE ?2 || '%'
                   AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
                   AND NOT EXISTS (
                     SELECT 1 FROM memory_index_nodes u
                     WHERE u.agent_id = t.agent_id AND u.node_type = 'unit'
                       AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id
                   )
                 ORDER BY t.created_at DESC LIMIT ?4",
            )
            .unwrap();
        let rows = stmt
            .query_map(
                params![agent_id, month_prefix, exclude_session_id, limit as i64],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    fn prod_ids(conn: &Connection) -> Vec<String> {
        list_undeclared_topic_nodes_for_month(conn, "a1", MONTH, "sess-excluded", 100)
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect()
    }

    /// 各シナリオのノードを流し込み、本番クエリと素の NOT EXISTS 版が **同じ id 列**を
    /// 返すことを確認する。境界ケース（topic 0 件 / unit 0 件 / end_log_id NULL / 全 topic が
    /// max_end より後）を 1 つずつ独立した DB で回す。
    #[test]
    fn rewrite_is_equivalent_to_naive_not_exists() {
        // (name, topics, units)
        let scenarios: Vec<(&str, Vec<IndexNodeRow>, Vec<IndexNodeRow>)> = vec![
            // 1) topic が 1 件も無い → 両方空
            ("no topics", vec![], vec![unit("u1", Some(1), Some(10))]),
            // 2) unit が 1 件も無い → MAX(end) が NULL。全 topic が未宣言（左辺は NULL で
            //    偽、NOT EXISTS が真）。両方とも全件返す。
            (
                "no units",
                vec![
                    topic("t-a", Some(1), Some(5), None, "2026-01-02T00:00:00Z"),
                    topic("t-b", Some(6), Some(9), None, "2026-01-03T00:00:00Z"),
                ],
                vec![],
            ),
            // 3) 全 topic が全 unit の end より後 → 左辺（短絡側）で全件確定。
            (
                "all topics after max_end (short-circuit side)",
                vec![
                    topic("t-a", Some(100), Some(110), None, "2026-01-02T00:00:00Z"),
                    topic("t-b", Some(111), Some(120), None, "2026-01-03T00:00:00Z"),
                ],
                vec![
                    unit("u1", Some(1), Some(50)),
                    unit("u2", Some(51), Some(99)),
                ],
            ),
            // 4) 混在: 覆われた topic は除外、隙間の topic は残る（NOT EXISTS まで回る側）、
            //    max_end より後の topic は短絡側で残る。
            (
                "mixed: covered / gap / after",
                vec![
                    topic("t-covered", Some(2), Some(4), None, "2026-01-02T00:00:00Z"),
                    topic(
                        "t-boundary",
                        Some(50),
                        Some(50),
                        None,
                        "2026-01-03T00:00:00Z",
                    ),
                    topic("t-gap", Some(31), Some(39), None, "2026-01-04T00:00:00Z"),
                    topic(
                        "t-after",
                        Some(200),
                        Some(210),
                        None,
                        "2026-01-05T00:00:00Z",
                    ),
                    // start_log_id == MAX(end)=99。u2 の端に重なるので naive では **除外**。
                    // 左辺は `> `（`>=` ではない）ので短絡せず NOT EXISTS で除外に落ちる。
                    // これで `>` → `>=` への変異を殺す。
                    topic(
                        "t-at-max",
                        Some(99),
                        Some(105),
                        None,
                        "2026-01-06T00:00:00Z",
                    ),
                ],
                vec![
                    unit("u1", Some(1), Some(30)),
                    unit("u2", Some(40), Some(99)),
                ],
            ),
            // 5) topic の end_log_id が NULL → overlap 比較が NULL になり NOT EXISTS 真。
            //    左辺の start>MAX は真になり得る（start が MAX 超のとき短絡）／偽のとき回る。
            (
                "topic end_log_id NULL",
                vec![
                    topic("t-nullend-lo", Some(5), None, None, "2026-01-02T00:00:00Z"),
                    topic(
                        "t-nullend-hi",
                        Some(500),
                        None,
                        None,
                        "2026-01-03T00:00:00Z",
                    ),
                    topic("t-nullstart", None, Some(5), None, "2026-01-04T00:00:00Z"),
                    topic("t-nullboth", None, None, None, "2026-01-05T00:00:00Z"),
                ],
                vec![unit("u1", Some(1), Some(100))],
            ),
            // 6) unit 側に NULL 端 → MAX は非 NULL 端だけ拾う。NULL 端 unit は overlap で
            //    決してマッチしない（両クエリで同じく無視される）。
            (
                "unit has NULL endpoints",
                vec![
                    topic("t-a", Some(2), Some(4), None, "2026-01-02T00:00:00Z"),
                    topic("t-b", Some(60), Some(70), None, "2026-01-03T00:00:00Z"),
                    topic("t-c", Some(200), Some(210), None, "2026-01-04T00:00:00Z"),
                ],
                vec![
                    unit("u-nullend", Some(1), None),
                    unit("u-nullstart", None, Some(500)),
                    unit("u-real", Some(1), Some(50)),
                ],
            ),
        ];

        for (name, topics, units) in scenarios {
            let conn = crate::init_memory().unwrap();
            for t in &topics {
                insert_index_node(&conn, t).unwrap();
            }
            for u in &units {
                insert_index_node(&conn, u).unwrap();
            }
            let got = prod_ids(&conn);
            let want = naive_undeclared(&conn, "a1", MONTH, "sess-excluded", 100);
            assert_eq!(got, want, "scenario `{name}`: prod != naive");
        }
    }

    /// 短絡した側（左辺 `start_log_id > MAX(end)` が真）を明示的に通す: unit があっても
    /// それより新しい topic は必ず返る。
    #[test]
    fn short_circuit_side_includes_topics_newer_than_all_units() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(&conn, &unit("u1", Some(1), Some(100))).unwrap();
        insert_index_node(
            &conn,
            &topic("t-new", Some(101), Some(120), None, "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(prod_ids(&conn), vec!["t-new".to_string()]);
    }

    /// 回った側（左辺が偽で NOT EXISTS を評価）を明示的に通す: max_end 以下の範囲で、
    /// 覆われた topic は除外・隙間の topic は残る。
    #[test]
    fn correlated_side_excludes_covered_keeps_gap() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(&conn, &unit("u1", Some(1), Some(30))).unwrap();
        insert_index_node(&conn, &unit("u2", Some(40), Some(100))).unwrap();
        insert_index_node(
            &conn,
            &topic("t-covered", Some(5), Some(10), None, "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &topic("t-gap", Some(31), Some(39), None, "2026-01-03T00:00:00Z"),
        )
        .unwrap();
        // どちらも start_log_id(5,31) <= MAX(end)=100 なので左辺は偽 → NOT EXISTS まで回る。
        assert_eq!(prod_ids(&conn), vec!["t-gap".to_string()]);
    }

    /// unit が 0 件（MAX(end) = NULL）でも全 topic が返る（早期 return しない）。
    #[test]
    fn no_units_returns_all_topics() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(
            &conn,
            &topic("t-a", Some(1), Some(5), None, "2026-01-02T00:00:00Z"),
        )
        .unwrap();
        insert_index_node(
            &conn,
            &topic("t-b", Some(6), Some(9), None, "2026-01-03T00:00:00Z"),
        )
        .unwrap();
        // created_at DESC 順
        assert_eq!(prod_ids(&conn), vec!["t-b".to_string(), "t-a".to_string()]);
    }

    /// topic が 0 件なら空。
    #[test]
    fn no_topics_returns_empty() {
        let conn = crate::init_memory().unwrap();
        insert_index_node(&conn, &unit("u1", Some(1), Some(10))).unwrap();
        assert!(prod_ids(&conn).is_empty());
    }

    /// #410 の効果計測用（既定では走らせない）。本番規模のシード DB を作り、旧
    /// （素の相関 NOT EXISTS）と新（MAX 短絡）の実測と EXPLAIN QUERY PLAN を印字する。
    /// 実行: `cargo test -p opencrab-db bench_undeclared -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_undeclared_old_vs_new() {
        use std::time::Instant;

        let conn = crate::init_memory().unwrap();
        // 本番 memory_index_nodes は約 8,400 行。ここでは同月・session_log の topic を
        // 主体に据え、宣言済み unit が古い側を覆い、新しい topic は未宣言（start > max_end）
        // という実運用の分布を作る。
        let n_units: i64 = 400; // 宣言ユニット（古い側を覆う）
        let n_topics_declared: i64 = 4000; // max_end 以下（覆われ or 隙間 → NOT EXISTS を通る）
        let n_topics_new: i64 = 4000; // max_end より後（短絡側で早期確定）

        // unit: log id 1..=(n_units*10) を 10 幅で連続に覆う（max_end = n_units*10）。
        let max_declared = n_units * 10;
        for i in 0..n_units {
            let s = i * 10 + 1;
            let e = s + 9;
            insert_index_node(&conn, &unit(&format!("u{i}"), Some(s), Some(e))).unwrap();
        }
        // 宣言域内の topic（覆われた / 隙間まちまち）。
        for i in 0..n_topics_declared {
            let s = i % max_declared + 1;
            let e = s + 3;
            insert_index_node(
                &conn,
                &topic(
                    &format!("td{i}"),
                    Some(s),
                    Some(e),
                    None,
                    &format!("2026-01-10T00:{:02}:{:02}Z", (i / 60) % 60, i % 60),
                ),
            )
            .unwrap();
        }
        // 宣言域より後（未宣言）の topic。
        for i in 0..n_topics_new {
            let s = max_declared + i * 5 + 1;
            let e = s + 4;
            insert_index_node(
                &conn,
                &topic(
                    &format!("tn{i}"),
                    Some(s),
                    Some(e),
                    None,
                    &format!("2026-01-20T00:{:02}:{:02}Z", (i / 60) % 60, i % 60),
                ),
            )
            .unwrap();
        }

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
            .unwrap();
        eprintln!(
            "--- seed: total rows={total}, units={n_units}, topics(declared)={n_topics_declared}, topics(new)={n_topics_new}, max_end={max_declared}"
        );

        // 結果が一致することも同時に確認（規模ありでの同値）。
        let got = prod_ids(&conn);
        let want = naive_undeclared(&conn, "a1", MONTH, "sess-excluded", 100);
        assert_eq!(got.len(), want.len(), "bench: prod/naive 件数不一致");
        assert_eq!(got, want, "bench: prod/naive 集合不一致");
        eprintln!("--- result rows (both) = {}", got.len());

        let new_sql = "SELECT t.id FROM memory_index_nodes t
             WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
               AND t.date_from LIKE ?2 || '%'
               AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
               AND (
                 t.start_log_id > (SELECT MAX(u.end_log_id) FROM memory_index_nodes u
                                   WHERE u.agent_id = ?1 AND u.node_type = 'unit')
                 OR NOT EXISTS (SELECT 1 FROM memory_index_nodes u
                   WHERE u.agent_id = ?1 AND u.node_type = 'unit'
                     AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id)
               )
             ORDER BY t.created_at DESC LIMIT ?4";
        let old_sql = "SELECT t.id FROM memory_index_nodes t
             WHERE t.agent_id = ?1 AND t.node_type = 'topic' AND t.source_type = 'session_log'
               AND t.date_from LIKE ?2 || '%'
               AND (t.source_session_id IS NULL OR t.source_session_id != ?3)
               AND NOT EXISTS (SELECT 1 FROM memory_index_nodes u
                 WHERE u.agent_id = t.agent_id AND u.node_type = 'unit'
                   AND u.start_log_id <= t.end_log_id AND u.end_log_id >= t.start_log_id)
             ORDER BY t.created_at DESC LIMIT ?4";

        let explain = |label: &str, sql: &str| {
            let eqp = format!("EXPLAIN QUERY PLAN {sql}");
            let mut stmt = conn.prepare(&eqp).unwrap();
            let rows: Vec<String> = stmt
                .query_map(params!["a1", MONTH, "sess-excluded", 100i64], |r| {
                    r.get::<_, String>(3)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            eprintln!("--- EXPLAIN QUERY PLAN [{label}]");
            for line in rows {
                eprintln!("    {line}");
            }
        };
        explain("OLD (correlated NOT EXISTS)", old_sql);
        explain("NEW (MAX short-circuit + NOT EXISTS)", new_sql);

        let run = |sql: &str| -> u128 {
            let mut stmt = conn.prepare(sql).unwrap();
            let iters = 20;
            let start = Instant::now();
            for _ in 0..iters {
                let rows = stmt
                    .query_map(params!["a1", MONTH, "sess-excluded", 100i64], |r| {
                        r.get::<_, String>(0)
                    })
                    .unwrap();
                let _: Vec<String> = rows.map(|r| r.unwrap()).collect();
            }
            start.elapsed().as_micros() / iters
        };
        // ウォームアップ
        run(old_sql);
        run(new_sql);
        let old_us = run(old_sql);
        let new_us = run(new_sql);
        eprintln!("--- timing (avg over 20 iters): OLD={old_us}us  NEW={new_us}us");
    }
}
