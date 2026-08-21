use anyhow::Result;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

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
