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
    // #425: around 窓の前後 radius 件からもエコー行を除外（記憶系で不可視・窓境界を一貫）。
    let lo: Option<i64> = conn.query_row(
        &format!(
            "SELECT MIN(id) FROM (SELECT id FROM memory_sessions
         WHERE agent_id = ?1 AND id <= ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}
         ORDER BY id DESC LIMIT ?3)"
        ),
        params![agent_id, center_id, radius + 1],
        |r| r.get(0),
    )?;
    let hi: Option<i64> = conn.query_row(
        &format!(
            "SELECT MAX(id) FROM (SELECT id FROM memory_sessions
         WHERE agent_id = ?1 AND id >= ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}
         ORDER BY id ASC LIMIT ?3)"
        ),
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
    // #425: エコー行（表示専用）は read_my_history の内容・range_total から除外する
    // （記憶系のあらゆる経路で不可視。パラメータを持たない述語なので where へ連結する）。
    where_parts.push(EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL.to_string());
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

