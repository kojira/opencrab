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

/// 新しい側から `limit` 件。`before_id` があるときはそれより小さい id。返り値は id ASC。
pub fn list_session_logs_page(
    conn: &Connection,
    session_id: &str,
    limit: u32,
    before_id: Option<i64>,
) -> Result<Vec<SessionLogRow>> {
    let sql = if before_id.is_some() {
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3"
    } else {
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2"
    };
    let mut stmt = conn.prepare(sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
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
    };
    let mut rows: Vec<SessionLogRow> = if let Some(before) = before_id {
        stmt.query_map(params![session_id, before, limit], map_row)?
            .collect::<std::result::Result<_, _>>()?
    } else {
        stmt.query_map(params![session_id, limit], map_row)?
            .collect::<std::result::Result<_, _>>()?
    };
    rows.reverse();
    Ok(rows)
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

/// 親ターンのiteration間へ注入する新着発言とsubtask completionを返す。
///
/// 発言の述語は[`list_user_speech_logs_after`]と同じ。completionは親sessionへ保存済みの
/// system eventだけを加える。単調増加idにより同じ行を二度返さない。
pub fn list_live_inbound_logs_after(
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
         WHERE session_id = ?1 AND id > ?3
           AND (
             (log_type = 'speech' AND speaker_id IS NOT NULL AND speaker_id != ?2
              AND (?4 IS NULL OR speaker_id = ?4))
             OR
             (log_type = 'system' AND
              CASE WHEN json_valid(content) THEN json_extract(content, '$.type') END = 'subtask_completed')
           )
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

/// 走行中サブタスクへ届いた steer（追加指示）ログを、`after_id` より後だけ古い順に返す（#647）。
///
/// `list_user_speech_logs_after` の steer 版。サブタスクは `run_agent_response` を depth+1 で
/// 再入し親と同じ engine ループを通るため、走行中注入（`LiveInboundSource`）を steer に流用
/// する。走行中サブの sub-session（`subtask-{id}`）に `log_type='steer'` で積まれた行だけを
/// 対象にし、通常発話や system ログは拾わない（steer は履歴上でも区別される / #647 記録要件）。
///
/// 呼び出し側（`SubtaskSteerInbound`）は前回取得の最大 id を `after_id` に渡す。単調増加の id
/// で同じ steer を二度注入しないことを保証する。`limit` は暴走時の安全弁で、超過分は次の
/// 呼び出しで拾われる。
pub fn list_steer_logs_after(
    conn: &Connection,
    session_id: &str,
    after_id: i64,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions
         WHERE session_id = ?1 AND log_type = 'steer'
           AND id > ?2
         ORDER BY id ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![session_id, after_id, limit as i64], |row| {
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

