use anyhow::Result;
use rusqlite::{params, Connection};

#[allow(unused_imports)]
use super::*;

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

/// index ツリーの「エージェントに 1 つだけ」持つ専用ルート（`node_type='root'`）の種別。
///
/// カテゴリ層（#313）/ 宣言（#379 #376）/ 凝縮（#411）はそれぞれ別ルートにぶら下がる。
/// 3 種は id 接頭辞・`source_type`・`title` だけが違い、確保ロジック（決定的 id を先に get →
/// 無ければ insert → read-back）は同一なので [`ensure_root`] に集約する。ルート種別を足す
/// ときはここに 1 バリアント加えるだけで、read-back ガード付きの生成経路に自動的に載る
/// （ガードの入れ忘れが構造的に起きない / #520）。`node_type` は常に `'root'`、short_id は
/// 常に `r` 系列なので**パラメータ化しない**（種別ごとに変わらないものは固定する）。
#[derive(Clone, Copy)]
pub(crate) enum RootKind {
    /// カテゴリ層のルート（`source_type='category'` / #313）。
    Category,
    /// 宣言した記憶のルート（`source_type='declared'` / #379 #376）。
    Declared,
    /// 凝縮した記憶のルート（`source_type='condensed'` / #411）。
    Condensed,
}

impl RootKind {
    /// 決定的 id の接頭辞。id は `<prefix>-<agent_id>`。
    fn id_prefix(self) -> &'static str {
        match self {
            RootKind::Category => "catroot",
            RootKind::Declared => "declroot",
            RootKind::Condensed => "condroot",
        }
    }

    /// `source_type` 列の値。ルート同士を id・source_type の両方で分離する。
    fn source_type(self) -> &'static str {
        match self {
            RootKind::Category => "category",
            RootKind::Declared => "declared",
            RootKind::Condensed => "condensed",
        }
    }

    /// 俯瞰表示に出るルート名。
    fn title(self) -> &'static str {
        match self {
            RootKind::Category => "カテゴリ",
            RootKind::Declared => "宣言した記憶",
            RootKind::Condensed => "凝縮した記憶",
        }
    }
}

/// 種別ごとの専用ルート（`node_type='root'`）を 1 つ確保して id を返す。
///
/// id はエージェント決定的（`<prefix>-<agent_id>`）なので「先に get → 無ければ insert」で
/// 冪等。各ルートは id も `source_type` も別（同じ agent の session_log 根 `root-<agent_id>`
/// とも混ざらない）。
///
/// insert は [`insert_index_node`] 経由＝`INSERT OR IGNORE` なので、short_id の UNIQUE 衝突や
/// `node_type` の CHECK 違反が起きても**エラーにならず黙って握り潰される**（#344 の教訓）。
/// そこで insert 後に id で read-back し、実在しなければ `bail` する。この 1 経路に 3 種
/// （将来の 4 種目も）が乗るので、ガードの入れ忘れが構造的に起きない（#520）。
///
/// 注: 現行の実入力ではこの握り潰しは発生しない（決定的 id を先頭で get して抜けるので PK
/// 衝突に至らず、short_id は `next_short_id` が MAX+1 を返すので UNIQUE 衝突せず、
/// `node_type='root'` は常に CHECK を通り、接続は単一で直列化され TOCTOU も無い）。read-back は
/// あくまで将来の退行（CHECK 縮小・short_id 採番変更など）に対する防御である。
pub(crate) fn ensure_root(
    conn: &Connection,
    agent_id: &str,
    now: &str,
    kind: RootKind,
) -> Result<String> {
    let id = format!("{}-{agent_id}", kind.id_prefix());
    if get_index_node(conn, &id)?.is_some() {
        return Ok(id);
    }
    let short_id = next_short_id(conn, agent_id, "r")?;
    let root = IndexNodeRow {
        id: id.clone(),
        agent_id: agent_id.to_string(),
        parent_id: None,
        node_type: "root".to_string(),
        source_type: kind.source_type().to_string(),
        title: kind.title().to_string(),
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
        anyhow::bail!(
            "ルート（{}）の作成に失敗しました（ノードが作成されませんでした）",
            kind.source_type()
        );
    }
    Ok(id)
}

/// カテゴリツリーの根（`node_type='root'`, `source_type='category'`）を確保して id を返す。
///
/// id はエージェント決定的（`catroot-<agent_id>`）なので冪等。既存の session_log ツリーの根
/// （`root-<agent_id>`）とは id も source_type も別なので混ざらない。実体は [`ensure_root`]。
pub fn ensure_category_root(conn: &Connection, agent_id: &str, now: &str) -> Result<String> {
    ensure_root(conn, agent_id, now, RootKind::Category)
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
pub(crate) fn uuid_like(agent_id: &str, title: &str, now: &str) -> String {
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
