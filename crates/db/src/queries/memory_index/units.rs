use anyhow::Result;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

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
/// id はエージェント決定的（`declroot-<agent_id>`）なので冪等。session_log ツリーの根
/// （`root-<agent_id>`）ともカテゴリ根（`catroot-<agent_id>`）とも id・source_type が別なので
/// 混ざらない。実体は [`ensure_root`]。
pub fn ensure_declared_root(conn: &Connection, agent_id: &str, now: &str) -> Result<String> {
    ensure_root(conn, agent_id, now, RootKind::Declared)
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
