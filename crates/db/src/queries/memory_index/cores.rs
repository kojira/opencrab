use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

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
    ensure_root(conn, agent_id, now, RootKind::Condensed)
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
