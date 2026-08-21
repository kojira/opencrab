use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

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
