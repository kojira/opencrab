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
        // #425: エコー行（表示専用）は地図・件数・est_tokens から除外（記憶系で不可視）。
        &format!(
            "SELECT COUNT(*), COUNT(DISTINCT session_id), MIN(id), MAX(id),
                    COALESCE(SUM(LENGTH(content)), 0)
                 FROM memory_sessions WHERE agent_id = ?1 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
        ),
        params![agent_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let total_buckets: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM (SELECT {bucket_expr} AS bkt FROM memory_sessions
             WHERE agent_id = ?1 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL} GROUP BY bkt)"
        ),
        params![agent_id],
        |r| r.get(0),
    )?;
    let mut buckets: Vec<HistoryBucket> = {
        let sql = format!(
            "SELECT {bucket_expr} AS bkt, COUNT(*), COUNT(DISTINCT session_id), MIN(id), MAX(id),
                    COALESCE(SUM(LENGTH(content)), 0)
             FROM memory_sessions WHERE agent_id = ?1 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}
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
               AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}
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

