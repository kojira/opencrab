use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

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
