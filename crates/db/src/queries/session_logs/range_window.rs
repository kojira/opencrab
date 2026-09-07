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
        // #425: 宣言範囲メタからエコー行を除外（宣言材料・件数を記憶系で不可視に一貫）。
        &format!(
            "SELECT COUNT(*), MIN(id), MAX(id), MIN(created_at), MAX(created_at)
         FROM memory_sessions WHERE agent_id = ?1 AND id >= ?2 AND id <= ?3
           AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
        ),
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
    // #425: 宣言窓の total_remaining・窓の中身からエコー行を除外（宣言経路で不可視に一貫）。
    let total_remaining: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM memory_sessions
         WHERE agent_id = ?1 AND id > ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
        ),
        params![agent_id, cursor_id],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT id, session_id, created_at FROM memory_sessions
         WHERE agent_id = ?1 AND id > ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}
         ORDER BY id ASC LIMIT ?3"
    ))?;
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
    // #425: 「生ログを N 件ぶん進める」窓境界の算出でもエコー行を数えない（宣言経路で
    // 不可視に一貫。エコーを 1 件に数えると窓境界が本来より手前へずれる）。
    let offset = n.max(1) - 1;
    let nth = conn.query_row(
        &format!(
            "SELECT id FROM memory_sessions
             WHERE agent_id = ?1 AND id > ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}
             ORDER BY id ASC LIMIT 1 OFFSET ?3"
        ),
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
        &format!(
            "SELECT MAX(id) FROM memory_sessions
         WHERE agent_id = ?1 AND id > ?2 AND {EXCLUDE_HEARTBEAT_CHANNEL_ECHO_SQL}"
        ),
        params![agent_id, cursor_id],
        |r| r.get(0),
    )?;
    Ok(last)
}
