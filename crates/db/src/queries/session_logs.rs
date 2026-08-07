use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// MEMORY: Sessions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogRow {
    pub id: Option<i64>,
    pub agent_id: String,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub speaker_id: Option<String>,
    pub turn_number: Option<i32>,
    pub metadata_json: Option<String>,
    pub created_at: Option<String>,
}

/// `insert_session_log` の best-effort 版: 失敗を握り潰さず warn を残す（#47）。
///
/// 会話履歴のクリティカル経路では挿入失敗が「無言の履歴欠落」になる。伝播すると
/// 応答フロー自体を止めてしまう場所（ログは副作用）で使う想定なので、エラーは
/// 返さずログのみ。戻り値が要る/失敗を伝播すべき場所では `insert_session_log` を使うこと。
pub fn insert_session_log_best_effort(conn: &Connection, log: &SessionLogRow) {
    if let Err(e) = insert_session_log(conn, log) {
        tracing::warn!(
            session_id = %log.session_id,
            log_type = %log.log_type,
            "session log insert failed (best-effort path): {e}"
        );
    }
}

pub fn insert_session_log(conn: &Connection, log: &SessionLogRow) -> Result<i64> {
    insert_session_log_at(conn, log, &Utc::now().to_rfc3339())
}

/// `created_at` を**呼び出し側が決める** [`insert_session_log`]（#413）。
///
/// 通常の記録経路は「いま起きたこと」を書くので `Utc::now()` で正しいが、過去ログの
/// **取り込み**では元の発生時刻でなければ意味が無い（宣言ランの窓も記憶索引の期間も
/// `created_at` で切る）。`SessionLogRow::created_at` を黙って使う形にしなかったのは、
/// 既存の全呼び出しが `None` を渡しており、意味を後付けで変えると「渡し忘れたら現在時刻」
/// という静かな分岐が生まれるため。時刻を持ち込む経路だけがこちらを名指しで呼ぶ。
///
/// `created_at` は**他の行と同じ表記**（`DateTime::to_rfc3339()`）で渡すこと。比較も
/// バケットも文字列で走るので、表記が混ざると順序が壊れる。
pub fn insert_session_log_at(
    conn: &Connection,
    log: &SessionLogRow,
    created_at: &str,
) -> Result<i64> {
    // 本体テーブルとFTS影テーブルへの2書き込みをトランザクションで原子化する。
    // 途中失敗で FTS と memory_sessions が恒久的に不整合になるのを防ぐ。
    //
    // **既に外側のトランザクション中なら、そちらの原子性に乗る**（#413）。SQLite は
    // `BEGIN` の入れ子を許さないので、まとめて入れたい呼び出し側（取り込みは全行を
    // 1 トランザクションにする — 途中で落ちた半端な範囲を残すと宣言ランのカーソルが
    // その途中を跨ぐ）から呼ぶと、ここで無条件に `BEGIN` すると失敗する。
    let tx = if conn.is_autocommit() {
        Some(conn.unchecked_transaction()?)
    } else {
        None
    };

    conn.execute(
        "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            log.agent_id,
            log.session_id,
            log.log_type,
            log.content,
            log.speaker_id,
            log.turn_number,
            log.metadata_json,
            created_at,
        ],
    )?;

    let row_id = conn.last_insert_rowid();

    // FTSにも追加
    conn.execute(
        "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            row_id,
            log.content,
            log.agent_id,
            log.session_id,
            log.log_type
        ],
    )?;

    if let Some(tx) = tx {
        tx.commit()?;
    }

    Ok(row_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogResult {
    pub id: i64,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub created_at: String,
    pub score: f64,
}

pub fn search_session_logs(
    conn: &Connection,
    agent_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionLogResult>> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    let fts_query = tokens.join(" AND ");

    let mut stmt = conn.prepare(
        "SELECT ms.id, ms.session_id, ms.log_type, ms.content, ms.created_at, bm25(memory_sessions_fts) as score
         FROM memory_sessions_fts fts
         JOIN memory_sessions ms ON fts.rowid = ms.id
         WHERE fts.agent_id = ?1 AND memory_sessions_fts MATCH ?2
         ORDER BY score
         LIMIT ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, fts_query, limit as i64], |row| {
        Ok(SessionLogResult {
            id: row.get(0)?,
            session_id: row.get(1)?,
            log_type: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
            score: row.get(5)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// クエリにマッチする生ログの**総件数**（LIMIT なし）。`search_my_history` の
/// estimate モードが「何件ヒットするか（絞るべきか）」を返すのに使う（#386）。
/// 検索式の組み立ては [`search_session_logs`] と同一。
pub fn count_matching_session_logs(conn: &Connection, agent_id: &str, query: &str) -> Result<i64> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    let fts_query = tokens.join(" AND ");
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions_fts fts
         WHERE fts.agent_id = ?1 AND memory_sessions_fts MATCH ?2",
        params![agent_id, fts_query],
        |r| r.get(0),
    )?;
    Ok(count)
}

/// List all session logs for a given session, ordered by creation time.
/// Used for building conversation history in send_message.
pub fn list_session_logs_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC",
    )?;

    let rows = stmt.query_map(params![session_id], |row| {
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

/// Count the number of logs in a session.
pub fn count_session_logs(conn: &Connection, session_id: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// List session logs with id > after_id, ordered by id ASC.
pub fn list_session_logs_after_id(
    conn: &Connection,
    session_id: &str,
    after_id: i64,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 AND id > ?2 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![session_id, after_id], |row| {
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

/// List the most recent N session logs (returned in id DESC order; caller should reverse).
pub fn list_recent_session_logs(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![session_id, limit as i64], |row| {
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

/// List the most recent N session logs **of one log_type** (returned in id DESC order;
/// caller should reverse).
///
/// [`list_recent_session_logs`] と同形で、`log_type` を SQL 側で絞るだけの違い。
/// 呼び出し側で絞ると「窓 N 件を取ってから捨てる」ことになり、ツール往復の多い
/// セッションでは目的の種別が N の一部しか残らない（#404 / #405 レビュー 2 巡目:
/// 生の 500 行から speech が 164 行しか残らず、遡れる期間が 2.2 日 → 0.9 日に縮んだ）。
pub fn list_recent_session_logs_of_type(
    conn: &Connection,
    session_id: &str,
    log_type: &str,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 AND log_type = ?2 ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, log_type, limit as i64], |row| {
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

/// List the most recent N **user** speech logs of a session (returned in id DESC order).
///
/// 「ユーザーの発言」= `log_type='speech'` かつ発話者がエージェント自身でない行。
/// #284: ツール往復でログが埋まると、単純な「直近 N 件」ではユーザー発言が 1 件も
/// 残らずプロンプトから消える。会話の再構築時に**必ず**混ぜ戻すために使う。
pub fn list_recent_user_speech_logs(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions
         WHERE session_id = ?1 AND log_type = 'speech'
           AND speaker_id IS NOT NULL AND speaker_id != ?2
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, agent_id, limit as i64], |row| {
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

/// 指定 id より**後**に記録されたユーザー発言を古い順（id ASC）に返す（#289）。
///
/// 走行中のターンへ新着だけを注入するための差分クエリ。「ユーザーの発言」の述語は
/// [`list_recent_user_speech_logs`] と同一（`log_type='speech'` かつ発話者が
/// `agent_id` 引数と異なる）で、両者は必ず一致させること。
///
/// 呼び出し側は前回取得した最大 id を `after_id` に渡す。同じ発言を二度返さない
/// のはこの単調増加の id によって保証される。`limit` は暴走時の安全弁で、超過分は
/// 次の呼び出しで拾われる（id は進むので取りこぼしはない）。
///
/// `only_speaker` を `Some(pk)` にすると、その `speaker_id` の発言だけへ絞る（#323 / B2）。
/// Nostr は 1 セッションに全相手が同居する（#323）ため、返信中の相手以外の新着を走行中に
/// 注入すると、返信先（`reply_target`）と食い違う本文を公開リレーへ誤爆させる。`None` なら
/// 従来どおり自分以外の全発言（Discord / heartbeat の既定）。
pub fn list_user_speech_logs_after(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    after_id: i64,
    only_speaker: Option<&str>,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions
         WHERE session_id = ?1 AND log_type = 'speech'
           AND speaker_id IS NOT NULL AND speaker_id != ?2
           AND id > ?3
           AND (?4 IS NULL OR speaker_id = ?4)
         ORDER BY id ASC LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![session_id, agent_id, after_id, only_speaker, limit as i64],
        |row| {
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
        },
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Get topic nodes for a specific session, ordered by start_log_id ASC.
pub fn get_topic_nodes_for_session(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS}
         FROM memory_index_nodes WHERE agent_id = ?1 AND source_session_id = ?2 AND node_type = 'topic' ORDER BY start_log_id ASC"
    ))?;
    let rows = stmt.query_map(params![agent_id, session_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// スリープ棚卸しトリガ用: 指定時刻以降にログを持つ distinct セッション数（新規活動量）。
/// `since` が None なら全期間。採点済み件数ではなく「未処理の活動量」を数える。
pub fn count_active_sessions_since(
    conn: &Connection,
    agent_id: &str,
    since: Option<&str>,
) -> Result<i64> {
    let n: i64 = match since {
        Some(ts) => conn.query_row(
            "SELECT COUNT(DISTINCT session_id) FROM memory_sessions
             WHERE agent_id = ?1 AND created_at > ?2",
            params![agent_id, ts],
            |r| r.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(DISTINCT session_id) FROM memory_sessions WHERE agent_id = ?1",
            params![agent_id],
            |r| r.get(0),
        )?,
    };
    Ok(n)
}

/// スリープ棚卸しの結末素材: エージェント単位で直近の verify 評価を新しい順に返す。
/// 戻り値は (session_id, content)。棚卸しではセッション単位の結末として提示する。
pub fn list_recent_evaluations_by_agent(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, content FROM memory_sessions
         WHERE agent_id = ?1 AND log_type = 'evaluation' ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// 生ログの俯瞰・範囲読み（記憶の単位 / issue #379 #376 段階1）
// ============================================
//
// エージェントが自分の生ログ（memory_sessions）を俯瞰し、範囲を読んで、まとまりを
// 宣言するための読み取り 2 種。**全クエリ `agent_id` 固定**（エージェント間で混ぜない）。
// 生ログは読むだけ（消さない・変えない）。読み取りは**有界**にする（687 発話の塊を
// 一度に吐かせない）: `read_my_history` は行数 + 総文字数のハードキャップ + カーソル。

/// 生ログ本文の**概算**トークン数を文字数から出す係数（#386）。
///
/// 地図（survey）は「どこに何がどれだけあるか」の当たりを付けるための道具で、全履歴を
/// tiktoken に掛けるのは高い。そこで content の**文字数**から概算する。本番コピーの実測
/// （最大 3 エージェント）で `tok/char` は 0.45〜0.60 だった。**過小評価は危険**（収まると
/// 思って読んで #294 で潰される）なので、実測上限より上の `2/3`（≈0.667）で丸め、
/// **やや多めに見積もる**。読み取り（`read_my_history` / `search_my_history`）は範囲が
/// 有界なので tiktoken で実測する（そちらは概算しない）。
fn approx_tokens_from_chars(chars: i64) -> i64 {
    chars.max(0) * 2 / 3
}

/// `survey_my_history` の 1 バケット分の集計（地図の 1 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryBucket {
    /// バケットキー（day=`YYYY-MM-DD` / hour=`YYYY-MM-DDTHH` / week=`YYYY-WNN`）。
    pub bucket: String,
    pub log_count: i64,
    pub session_count: i64,
    pub min_id: i64,
    pub max_id: i64,
    /// このバケットの content 総文字数（`SUM(LENGTH(content))`）。
    pub content_chars: i64,
    /// content_chars からの**概算**トークン数（[`approx_tokens_from_chars`]）。
    /// この範囲を `read_my_history` で読むとおよそ何トークン積むかの目安。多めに見積もる。
    pub est_tokens: i64,
    /// log_type 別の件数（種別内訳）。
    pub type_counts: std::collections::BTreeMap<String, i64>,
}

/// `survey_my_history` の結果（地図）。集計なので小さいが、バケット数には上限を設ける。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySurvey {
    pub granularity: String,
    pub total_logs: i64,
    pub total_sessions: i64,
    pub min_id: Option<i64>,
    pub max_id: Option<i64>,
    /// 全 content の総文字数（バケットを落としても全体量が分かるよう常に返す）。
    pub total_content_chars: i64,
    /// total_content_chars からの**概算**トークン数（全履歴を読む場合の目安）。
    pub total_est_tokens: i64,
    pub total_buckets: i64,
    pub returned_buckets: usize,
    /// バケット数上限で古いバケットを落としたか。
    pub truncated: bool,
    /// 新しいバケットから最大 `max_buckets` 件。
    pub buckets: Vec<HistoryBucket>,
}

/// 生ログを日/時/週で集計して地図を返す（件数・セッション数・id 範囲・種別内訳）。
///
/// `granularity`: `"day"`（既定）/ `"hour"` / `"week"`。バケットは新しい順に最大
/// `max_buckets` 件返す（それより古いバケットは `total_buckets` に件数だけ残して落とす）。
/// 全体の総件数・総セッション数・id 範囲は（バケットを落としても）常に返す。
pub fn survey_my_history(
    conn: &Connection,
    agent_id: &str,
    granularity: &str,
    max_buckets: usize,
) -> Result<HistorySurvey> {
    // バケット式は string リテラルのみ（ユーザ入力を SQL へ入れない）。
    let bucket_expr = match granularity {
        "hour" => "substr(created_at, 1, 13)",
        "week" => "strftime('%Y-W%W', created_at)",
        _ => "substr(created_at, 1, 10)", // day（既定）
    };
    let (total_logs, total_sessions, min_id, max_id, total_content_chars): (
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        i64,
    ) = conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT session_id), MIN(id), MAX(id),
                COALESCE(SUM(LENGTH(content)), 0)
             FROM memory_sessions WHERE agent_id = ?1",
        params![agent_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let total_buckets: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM (SELECT {bucket_expr} AS bkt FROM memory_sessions
             WHERE agent_id = ?1 GROUP BY bkt)"
        ),
        params![agent_id],
        |r| r.get(0),
    )?;
    let mut buckets: Vec<HistoryBucket> = {
        let sql = format!(
            "SELECT {bucket_expr} AS bkt, COUNT(*), COUNT(DISTINCT session_id), MIN(id), MAX(id),
                    COALESCE(SUM(LENGTH(content)), 0)
             FROM memory_sessions WHERE agent_id = ?1
             GROUP BY bkt ORDER BY bkt DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![agent_id, max_buckets as i64], |r| {
            let content_chars: i64 = r.get(5)?;
            Ok(HistoryBucket {
                bucket: r.get(0)?,
                log_count: r.get(1)?,
                session_count: r.get(2)?,
                min_id: r.get(3)?,
                max_id: r.get(4)?,
                content_chars,
                est_tokens: approx_tokens_from_chars(content_chars),
                type_counts: std::collections::BTreeMap::new(),
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let truncated = (total_buckets as usize) > buckets.len();
    // 種別内訳は「保持したバケット」だけに絞って引く（最古の保持バケット以降）。
    if let Some(min_kept) = buckets.iter().map(|b| b.bucket.clone()).min() {
        let idx: std::collections::HashMap<String, usize> = buckets
            .iter()
            .enumerate()
            .map(|(i, b)| (b.bucket.clone(), i))
            .collect();
        let sql = format!(
            "SELECT {bucket_expr} AS bkt, log_type, COUNT(*)
             FROM memory_sessions WHERE agent_id = ?1 AND {bucket_expr} >= ?2
             GROUP BY bkt, log_type"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![agent_id, min_kept], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (bkt, log_type, c) = row?;
            if let Some(&i) = idx.get(&bkt) {
                buckets[i].type_counts.insert(log_type, c);
            }
        }
    }
    Ok(HistorySurvey {
        granularity: granularity.to_string(),
        total_logs,
        total_sessions,
        min_id,
        max_id,
        total_content_chars,
        total_est_tokens: approx_tokens_from_chars(total_content_chars),
        total_buckets,
        returned_buckets: buckets.len(),
        truncated,
        buckets,
    })
}

/// `read_my_history` の読み取り範囲指定。どれも `agent_id` でスコープされる。
pub enum HistoryFilter {
    /// セッション単位。
    Session(String),
    /// id 範囲 `[from_id, to_id]`（順不同でも正規化する）。
    IdRange { from_id: i64, to_id: i64 },
    /// 時刻範囲 `[from_time, to_time]`（RFC3339 の文字列比較）。
    TimeRange { from_time: String, to_time: String },
    /// ある id の前後 `radius` 件（このエージェントの id 順で前後）。
    Around { center_id: i64, radius: i64 },
}

/// `read_my_history` の 1 ページ分（有界）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    pub rows: Vec<SessionLogRow>,
    /// 範囲全体の総件数（キャップ前）。「687 発話ある」を伝えるため常に返す。
    pub range_total: i64,
    pub returned: usize,
    /// 行数 or 文字数キャップで打ち切ったか。
    pub truncated: bool,
    /// 打ち切った場合の続き先頭 id（このエージェントの次の未返却行）。
    pub next_from_id: Option<i64>,
}

/// `Around` を id 範囲へ解決する（このエージェントの id 順で center の前後 radius 件）。
/// 該当が無ければ center 自身にフォールバック。
fn resolve_around_window(
    conn: &Connection,
    agent_id: &str,
    center_id: i64,
    radius: i64,
) -> Result<(i64, i64)> {
    let lo: Option<i64> = conn.query_row(
        "SELECT MIN(id) FROM (SELECT id FROM memory_sessions
         WHERE agent_id = ?1 AND id <= ?2 ORDER BY id DESC LIMIT ?3)",
        params![agent_id, center_id, radius + 1],
        |r| r.get(0),
    )?;
    let hi: Option<i64> = conn.query_row(
        "SELECT MAX(id) FROM (SELECT id FROM memory_sessions
         WHERE agent_id = ?1 AND id >= ?2 ORDER BY id ASC LIMIT ?3)",
        params![agent_id, center_id, radius + 1],
        |r| r.get(0),
    )?;
    Ok((lo.unwrap_or(center_id), hi.unwrap_or(center_id)))
}

/// 生ログを範囲指定で読む（**有界**: 行数キャップ + 総文字数キャップ + カーソル）。
///
/// `cursor_from_id` を渡すとその id 以降だけを読む（続きの取得）。行数 `row_cap` を超える、
/// または本文の累計が `char_cap` を超える手前で打ち切り、`truncated` と `next_from_id`
/// （続き先頭）を返す。先頭 1 行は文字数キャップに関係なく必ず返す（前進保証・巨大 1 行で
/// 詰まらせない）。`range_total` は範囲全体の件数（キャップ前）を常に返す。生ログは読むだけ。
pub fn read_my_history(
    conn: &Connection,
    agent_id: &str,
    filter: &HistoryFilter,
    cursor_from_id: Option<i64>,
    row_cap: usize,
    char_cap: usize,
) -> Result<HistoryPage> {
    use rusqlite::types::ToSql;

    let mut where_parts: Vec<String> = vec!["agent_id = ?1".to_string()];
    let mut p: Vec<Box<dyn ToSql>> = vec![Box::new(agent_id.to_string())];
    match filter {
        HistoryFilter::Session(sid) => {
            let idx = p.len() + 1;
            where_parts.push(format!("session_id = ?{idx}"));
            p.push(Box::new(sid.clone()));
        }
        HistoryFilter::IdRange { from_id, to_id } => {
            let (lo, hi) = if from_id <= to_id {
                (*from_id, *to_id)
            } else {
                (*to_id, *from_id)
            };
            let a = p.len() + 1;
            p.push(Box::new(lo));
            let b = p.len() + 1;
            p.push(Box::new(hi));
            where_parts.push(format!("id >= ?{a} AND id <= ?{b}"));
        }
        HistoryFilter::TimeRange { from_time, to_time } => {
            let a = p.len() + 1;
            p.push(Box::new(from_time.clone()));
            let b = p.len() + 1;
            p.push(Box::new(to_time.clone()));
            where_parts.push(format!("created_at >= ?{a} AND created_at <= ?{b}"));
        }
        HistoryFilter::Around { center_id, radius } => {
            let (lo, hi) = resolve_around_window(conn, agent_id, *center_id, *radius)?;
            let a = p.len() + 1;
            p.push(Box::new(lo));
            let b = p.len() + 1;
            p.push(Box::new(hi));
            where_parts.push(format!("id >= ?{a} AND id <= ?{b}"));
        }
    }
    let base_where = where_parts.join(" AND ");

    let range_total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM memory_sessions WHERE {base_where}");
        let refs: Vec<&dyn ToSql> = p.iter().map(|b| b.as_ref()).collect();
        conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?
    };

    // カーソル + 行数キャップ+1（+1 で「まだ残りがある」を検出）。
    let cursor_idx = p.len() + 1;
    p.push(Box::new(cursor_from_id));
    let limit_idx = p.len() + 1;
    p.push(Box::new((row_cap as i64) + 1));
    let read_sql = format!(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions
         WHERE {base_where} AND (?{cursor_idx} IS NULL OR id >= ?{cursor_idx})
         ORDER BY id ASC LIMIT ?{limit_idx}"
    );
    let fetched: Vec<SessionLogRow> = {
        let refs: Vec<&dyn ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&read_sql)?;
        let rows = stmt.query_map(refs.as_slice(), |row| {
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
        rows.collect::<std::result::Result<_, _>>()?
    };

    let mut out: Vec<SessionLogRow> = Vec::new();
    let mut chars = 0usize;
    let mut truncated = false;
    let mut next_from_id: Option<i64> = None;
    for (i, row) in fetched.iter().enumerate() {
        if i >= row_cap {
            // 行数キャップ超過を検出する +1 行目 = まだ残りがある。
            truncated = true;
            next_from_id = row.id;
            break;
        }
        let c = row.content.chars().count();
        if i > 0 && chars + c > char_cap {
            // 先頭以外で総文字数キャップ超過 → ここから続き。
            truncated = true;
            next_from_id = row.id;
            break;
        }
        chars += c;
        out.push(row.clone());
    }
    let returned = out.len();
    Ok(HistoryPage {
        rows: out,
        range_total,
        returned,
        truncated,
        next_from_id,
    })
}

/// 宣言範囲のメタ情報（このエージェントの生ログに限定）。`record_memory_unit` が
/// 「範囲にこのエージェントのログが実在するか」を確認し、date_from/date_to を埋めるのに使う。
#[derive(Debug, Clone)]
pub struct LogRangeMeta {
    pub count: i64,
    pub min_id: i64,
    pub max_id: i64,
    pub min_created_at: String,
    pub max_created_at: String,
}

/// `[from_id, to_id]`（順不同可）にあるこのエージェントの生ログのメタを返す。
/// 範囲に 1 件も無ければ `None`（＝他エージェントの id や空範囲を宣言させない）。
pub fn log_range_meta(
    conn: &Connection,
    agent_id: &str,
    from_id: i64,
    to_id: i64,
) -> Result<Option<LogRangeMeta>> {
    let (lo, hi) = if from_id <= to_id {
        (from_id, to_id)
    } else {
        (to_id, from_id)
    };
    let (count, min_id, max_id, min_ts, max_ts): (
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT COUNT(*), MIN(id), MAX(id), MIN(created_at), MAX(created_at)
         FROM memory_sessions WHERE agent_id = ?1 AND id >= ?2 AND id <= ?3",
        params![agent_id, lo, hi],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(LogRangeMeta {
        count,
        min_id: min_id.unwrap_or(lo),
        max_id: max_id.unwrap_or(hi),
        min_created_at: min_ts.unwrap_or_default(),
        max_created_at: max_ts.unwrap_or_default(),
    }))
}

/// 宣言ラン（#384 / #376 段階2）が 1 回で提示する「未宣言の枠」。
///
/// マーカー（`memory_declare_cursor` の位置部）より新しい生ログを昇順（最古）から
/// `limit` 件だけ切り出した窓。中身（本文）は含めない — **地図（集計）だけ**を渡す設計
/// （要約を渡すと本人が読まない / #313 の実測）。本文はエージェントが `read_my_history` で
/// 自分で読む。窓が空（未宣言ログ 0）なら `from_id`/`to_id` は `None`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclareWindow {
    /// 窓の下端（このエージェントの生ログ id）。未宣言ログが無ければ `None`。
    pub from_id: Option<i64>,
    /// 窓の上端。clean 完了時にマーカーの位置部をここへ前進させる。
    pub to_id: Option<i64>,
    /// 窓に入った生ログ件数（`<= limit`）。
    pub log_count: i64,
    /// 窓に含まれるセッション数（切れ目の目安）。
    pub session_count: i64,
    /// マーカーより新しい生ログの総数（窓で切る前）。発火の下限ゲートに使う。
    pub total_remaining: i64,
    /// 窓の開始時刻（最古行の created_at）。
    pub date_from: Option<String>,
    /// 窓の終了時刻（最新行の created_at）。
    pub date_to: Option<String>,
}

/// マーカー位置 `cursor_id`（この id は宣言済みとして除外）より新しい生ログの窓を返す。
///
/// **前進のみ**の設計: 窓は id 昇順で `cursor_id` の次から `limit` 件。「どの生ログが既に
/// 宣言ユニットに含まれるか」は判定条件にしない（提示したら位置を進める＝一期一会。意図的に
/// 宣言しなかった範囲を毎回拾い直さない / タグ整理ランと同じ流儀）。全クエリ `agent_id` 固定
/// （他エージェントの記憶を混ぜない）。生ログは読むだけ。
pub fn declare_window(
    conn: &Connection,
    agent_id: &str,
    cursor_id: i64,
    limit: i64,
) -> Result<DeclareWindow> {
    let total_remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = ?1 AND id > ?2",
        params![agent_id, cursor_id],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, session_id, created_at FROM memory_sessions
         WHERE agent_id = ?1 AND id > ?2
         ORDER BY id ASC LIMIT ?3",
    )?;
    let rows: Vec<(i64, String, String)> = stmt
        .query_map(params![agent_id, cursor_id, limit.max(1)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    if rows.is_empty() {
        return Ok(DeclareWindow {
            from_id: None,
            to_id: None,
            log_count: 0,
            session_count: 0,
            total_remaining,
            date_from: None,
            date_to: None,
        });
    }

    let from_id = rows.first().map(|r| r.0);
    let to_id = rows.last().map(|r| r.0);
    let date_from = rows.first().map(|r| r.2.clone());
    let date_to = rows.last().map(|r| r.2.clone());
    let mut sessions: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (_, sid, _) in &rows {
        sessions.insert(sid.as_str());
    }
    Ok(DeclareWindow {
        from_id,
        to_id,
        log_count: rows.len() as i64,
        session_count: sessions.len() as i64,
        total_remaining,
        date_from,
        date_to,
    })
}

/// `cursor_id` より新しい生ログを id 昇順に並べたときの **`n` 番目（1 始まり）の id**。
/// `n` 件も無ければ**最後（最大 id）**を、1 件も無ければ `None` を返す。
///
/// 宣言ラン（#394）が、本人の指定したカーソル位置を丸める**下限・上限**を作るために使う。
/// 「id を N 足す」ではなく「**生ログを N 件ぶん進める**」でなければ意味が無い（id は全
/// エージェント共通の採番で、1 エージェントぶんの間隔は疎らだから）。生ログは読むだけ。
pub fn nth_log_id_after(
    conn: &Connection,
    agent_id: &str,
    cursor_id: i64,
    n: i64,
) -> Result<Option<i64>> {
    let offset = n.max(1) - 1;
    let nth = conn.query_row(
        "SELECT id FROM memory_sessions
             WHERE agent_id = ?1 AND id > ?2
             ORDER BY id ASC LIMIT 1 OFFSET ?3",
        params![agent_id, cursor_id, offset],
        |r| r.get::<_, i64>(0),
    );
    match nth {
        Ok(id) => return Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(e.into()),
    }
    // n 件に満たない: あるだけ進める（＝最後の id）。1 件も無ければ None。
    let last: Option<i64> = conn.query_row(
        "SELECT MAX(id) FROM memory_sessions WHERE agent_id = ?1 AND id > ?2",
        params![agent_id, cursor_id],
        |r| r.get(0),
    )?;
    Ok(last)
}
