use rusqlite::{params, Connection, OptionalExtension};

pub fn next_tool_call_sequence(conn: &Connection, session_id: &str) -> rusqlite::Result<usize> {
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM tool_call_correlations WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    Ok(max.saturating_add(1) as usize)
}

pub fn insert_tool_call_correlation(
    conn: &Connection,
    session_id: &str,
    sequence: usize,
    short_id: &str,
    provider_call_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tool_call_correlations
         (session_id, sequence, short_id, provider_call_id, created_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        params![session_id, sequence as i64, short_id, provider_call_id],
    )?;
    Ok(())
}

pub fn resolve_tool_call_correlation(
    conn: &Connection,
    session_id: &str,
    provider_call_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT short_id FROM tool_call_correlations
         WHERE session_id = ?1 AND provider_call_id = ?2",
        params![session_id, provider_call_id],
        |row| row.get(0),
    )
    .optional()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCompletionState {
    Queued,
    Included,
    Consumed,
}

impl ToolCompletionState {
    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "included" => Ok(Self::Included),
            "consumed" => Ok(Self::Consumed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("invalid tool completion state: {other}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToolCompletionEvent<'a> {
    pub event_id: &'a str,
    pub session_id: &'a str,
    pub causal_turn_id: &'a str,
    pub tool_call_id: &'a str,
    pub execution_id: &'a str,
    pub result_log_id: i64,
    pub completed_at: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCompletionEventRow {
    pub event_id: String,
    pub session_id: String,
    pub causal_turn_id: String,
    pub tool_call_id: String,
    pub execution_id: String,
    pub result_log_id: i64,
    pub state: ToolCompletionState,
    pub included_request_id: Option<String>,
    pub request_digest: Option<String>,
    pub completed_at: String,
    pub consumed_at: Option<String>,
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolCompletionEventRow> {
    Ok(ToolCompletionEventRow {
        event_id: row.get(0)?,
        session_id: row.get(1)?,
        causal_turn_id: row.get(2)?,
        tool_call_id: row.get(3)?,
        execution_id: row.get(4)?,
        result_log_id: row.get(5)?,
        state: ToolCompletionState::parse(&row.get::<_, String>(6)?)?,
        included_request_id: row.get(7)?,
        request_digest: row.get(8)?,
        completed_at: row.get(9)?,
        consumed_at: row.get(10)?,
    })
}

const SELECT_COLUMNS: &str = "event_id, session_id, causal_turn_id, tool_call_id, execution_id, result_log_id, state, included_request_id, request_digest, completed_at, consumed_at";

pub fn enqueue_tool_completion_event(
    conn: &Connection,
    event: &NewToolCompletionEvent<'_>,
) -> rusqlite::Result<ToolCompletionEventRow> {
    conn.execute(
        "INSERT OR IGNORE INTO tool_completion_events
         (event_id, session_id, causal_turn_id, tool_call_id, execution_id,
          result_log_id, state, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7)",
        params![
            event.event_id,
            event.session_id,
            event.causal_turn_id,
            event.tool_call_id,
            event.execution_id,
            event.result_log_id,
            event.completed_at,
        ],
    )?;
    let found = conn.query_row(
        &format!(
            "SELECT {SELECT_COLUMNS} FROM tool_completion_events
             WHERE execution_id = ?1 AND tool_call_id = ?2"
        ),
        params![event.execution_id, event.tool_call_id],
        read_row,
    )?;
    if found.session_id != event.session_id
        || found.causal_turn_id != event.causal_turn_id
        || found.tool_call_id != event.tool_call_id
        || found.result_log_id != event.result_log_id
    {
        return Err(rusqlite::Error::InvalidParameterName(
            "execution_id reused with different completion correlation".to_string(),
        ));
    }
    Ok(found)
}

pub fn is_tool_completion_consumed(
    conn: &Connection,
    execution_id: &str,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) > 0 AND
                SUM(CASE WHEN state != 'consumed' THEN 1 ELSE 0 END) = 0
         FROM tool_completion_events WHERE execution_id = ?1",
        [execution_id],
        |row| row.get(0),
    )
}

pub fn list_unconsumed_tool_completion_events(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<ToolCompletionEventRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLUMNS} FROM tool_completion_events
         WHERE session_id = ?1 AND state != 'consumed'
         ORDER BY completed_at, event_id"
    ))?;
    let rows = stmt.query_map([session_id], read_row)?.collect();
    rows
}

fn load_event(
    conn: &Connection,
    event_id: &str,
) -> rusqlite::Result<Option<ToolCompletionEventRow>> {
    conn.query_row(
        &format!("SELECT {SELECT_COLUMNS} FROM tool_completion_events WHERE event_id = ?1"),
        [event_id],
        read_row,
    )
    .optional()
}

pub fn load_included_tool_completion_request(
    conn: &Connection,
    event_ids: &[String],
) -> rusqlite::Result<Option<(String, String)>> {
    let Some(first_id) = event_ids.first() else {
        return Ok(None);
    };
    let Some(first) = load_event(conn, first_id)? else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    if first.state != ToolCompletionState::Included {
        return Ok(None);
    }
    let request_id = first
        .included_request_id
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    for event_id in event_ids.iter().skip(1) {
        let event = load_event(conn, event_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if event.state != ToolCompletionState::Included
            || event.included_request_id.as_deref() != Some(&request_id)
        {
            return Err(rusqlite::Error::InvalidParameterName(
                "completion events do not share one included request".to_string(),
            ));
        }
    }
    conn.query_row(
        "SELECT request_json FROM tool_completion_requests WHERE request_id = ?1",
        [&request_id],
        |row| row.get(0),
    )
    .map(|request_json| Some((request_id, request_json)))
}

pub fn mark_tool_completion_events_included(
    conn: &Connection,
    event_ids: &[String],
    request_id: &str,
    request_digest: &str,
    request_json: &str,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let first_event_id = event_ids
        .first()
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let session_id = load_event(&tx, first_event_id)?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?
        .session_id;
    tx.execute(
        "INSERT OR IGNORE INTO tool_completion_requests
         (request_id, session_id, request_digest, request_json, effects_state, created_at)
         VALUES (?1, ?2, ?3, ?4, 'awaiting_response', datetime('now'))",
        params![request_id, session_id, request_digest, request_json],
    )?;
    for event_id in event_ids {
        let event = load_event(&tx, event_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        match event.state {
            ToolCompletionState::Queued => {
                tx.execute(
                    "UPDATE tool_completion_events
                     SET state = 'included', included_request_id = ?2, request_digest = ?3
                     WHERE event_id = ?1 AND state = 'queued'",
                    params![event_id, request_id, request_digest],
                )?;
            }
            ToolCompletionState::Included
                if event.request_digest.as_deref() == Some(request_digest)
                    && event.included_request_id.as_deref() == Some(request_id) => {}
            _ => {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "completion event {event_id} cannot enter included"
                )))
            }
        }
    }
    tx.commit()
}

pub fn record_tool_completion_response_in_transaction(
    conn: &Connection,
    request_id: &str,
    response_json: &str,
) -> rusqlite::Result<()> {
    let changed = conn.execute(
        "UPDATE tool_completion_requests
         SET response_json = ?2, effects_state = 'pending'
         WHERE request_id = ?1 AND effects_state = 'awaiting_response'",
        params![request_id, response_json],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}

pub fn load_pending_tool_completion_effect(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<(String, String, String)>> {
    conn.query_row(
        "SELECT request_id, request_json, response_json
         FROM tool_completion_requests
         WHERE session_id = ?1 AND effects_state = 'pending'
         ORDER BY created_at, request_id LIMIT 1",
        [session_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
}

pub fn mark_tool_completion_effect_applied(
    conn: &Connection,
    request_id: &str,
) -> rusqlite::Result<()> {
    let changed = conn.execute(
        "UPDATE tool_completion_requests
         SET effects_state = 'applied', applied_at = datetime('now')
         WHERE request_id = ?1 AND effects_state = 'pending'",
        [request_id],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}

pub fn mark_tool_completion_events_consumed_in_transaction(
    conn: &Connection,
    event_ids: &[String],
    request_id: &str,
) -> rusqlite::Result<()> {
    for event_id in event_ids {
        let event = load_event(conn, event_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        match event.state {
            ToolCompletionState::Included
                if event.included_request_id.as_deref() == Some(request_id) =>
            {
                conn.execute(
                    "UPDATE tool_completion_events
                     SET state = 'consumed', consumed_at = datetime('now')
                     WHERE event_id = ?1 AND state = 'included'",
                    [event_id],
                )?;
                // 現在の因果的turnでは本文を使った後、以後のturnは参照だけにする。
                let (_content, raw_meta): (String, Option<String>) = conn.query_row(
                    "SELECT content, metadata_json FROM memory_sessions WHERE id = ?1",
                    [event.result_log_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let mut meta = raw_meta
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .filter(serde_json::Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({}));
                let fields = meta.as_object_mut().expect("object checked above");
                fields.insert("result_omitted".to_string(), serde_json::json!(true));
                if fields
                    .get("result_path")
                    .is_none_or(|value| value.is_null())
                {
                    fields.insert(
                        "result_path".to_string(),
                        serde_json::json!(format!(
                            "read_my_history(around_id={})",
                            event.result_log_id
                        )),
                    );
                }
                conn.execute(
                    "UPDATE memory_sessions SET metadata_json = ?2 WHERE id = ?1",
                    params![event.result_log_id, meta.to_string()],
                )?;
            }
            ToolCompletionState::Consumed
                if event.included_request_id.as_deref() == Some(request_id) => {}
            _ => {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "completion event {event_id} cannot enter consumed"
                )))
            }
        }
    }
    Ok(())
}

pub fn mark_tool_completion_events_consumed(
    conn: &Connection,
    event_ids: &[String],
    request_id: &str,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    mark_tool_completion_events_consumed_in_transaction(&tx, event_ids, request_id)?;
    tx.commit()
}

/// exact response replay時、同じ会話用tool IDで既に永続化した結果を再利用する。
pub fn load_persisted_tool_effect(
    conn: &Connection,
    session_id: &str,
    tool_call_id: &str,
) -> rusqlite::Result<Option<(String, bool, String)>> {
    conn.query_row(
        "SELECT content,
                COALESCE(json_extract(metadata_json, '$.is_error'), 0),
                COALESCE(json_extract(metadata_json, '$.lifecycle_status'), 'completed')
         FROM memory_sessions
         WHERE session_id = ?1 AND log_type = 'tool_result'
           AND json_extract(metadata_json, '$.conversation_tool_id') = ?2
         ORDER BY id DESC LIMIT 1",
        params![session_id, tool_call_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = crate::init_memory().unwrap();
        conn.execute(
            "INSERT INTO memory_sessions
             (id, agent_id, session_id, log_type, content, created_at)
             VALUES (42, 'agent-1', 'session-1', 'tool_result', 'done',
                     '2026-09-08T00:00:00Z')",
            [],
        )
        .unwrap();
        conn
    }

    fn event<'a>(event_id: &'a str, execution_id: &'a str) -> NewToolCompletionEvent<'a> {
        NewToolCompletionEvent {
            event_id,
            session_id: "session-1",
            causal_turn_id: "turn-1",
            tool_call_id: "t1",
            execution_id,
            result_log_id: 42,
            completed_at: "2026-09-08T00:00:00Z",
        }
    }

    #[test]
    fn completion_state_machine_is_forward_only_and_duplicate_delivery_is_idempotent() {
        let conn = setup();
        let first = enqueue_tool_completion_event(&conn, &event("event-1", "exec-1")).unwrap();
        let replay =
            enqueue_tool_completion_event(&conn, &event("event-replay", "exec-1")).unwrap();
        assert_eq!(replay.event_id, first.event_id);

        let ids = vec!["event-1".to_string()];
        assert!(mark_tool_completion_events_consumed(&conn, &ids, "request-1").is_err());
        mark_tool_completion_events_included(&conn, &ids, "request-1", "digest-1", "{}").unwrap();
        mark_tool_completion_events_included(&conn, &ids, "request-1", "digest-1", "{}").unwrap();
        assert!(
            mark_tool_completion_events_included(&conn, &ids, "request-2", "digest-2", "{}")
                .is_err()
        );
        mark_tool_completion_events_consumed(&conn, &ids, "request-1").unwrap();
        mark_tool_completion_events_consumed(&conn, &ids, "request-1").unwrap();
        assert!(list_unconsumed_tool_completion_events(&conn, "session-1")
            .unwrap()
            .is_empty());
        let metadata: String = conn
            .query_row(
                "SELECT metadata_json FROM memory_sessions WHERE id = 42",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(metadata["result_omitted"], true);
        assert_eq!(metadata["result_path"], "read_my_history(around_id=42)");
        assert!(metadata.get("result_bytes").is_none());
        assert!(metadata.get("result_lines").is_none());
    }

    #[test]
    fn response_outbox_and_consumption_commit_together_then_apply() {
        let conn = setup();
        enqueue_tool_completion_event(&conn, &event("event-1", "exec-1")).unwrap();
        let ids = vec!["event-1".to_string()];
        mark_tool_completion_events_included(
            &conn,
            &ids,
            "request-1",
            "digest-1",
            r#"{"model":"test"}"#,
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        record_tool_completion_response_in_transaction(&tx, "request-1", r#"{"choices":[]}"#)
            .unwrap();
        mark_tool_completion_events_consumed_in_transaction(&tx, &ids, "request-1").unwrap();
        tx.commit().unwrap();

        let pending = load_pending_tool_completion_effect(&conn, "session-1")
            .unwrap()
            .unwrap();
        assert_eq!(pending.0, "request-1");
        assert_eq!(pending.1, r#"{"model":"test"}"#);
        assert_eq!(pending.2, r#"{"choices":[]}"#);
        mark_tool_completion_effect_applied(&conn, "request-1").unwrap();
        assert!(load_pending_tool_completion_effect(&conn, "session-1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn llm_log_and_consumption_rollback_together() {
        let conn = setup();
        enqueue_tool_completion_event(&conn, &event("event-1", "exec-1")).unwrap();
        mark_tool_completion_events_included(
            &conn,
            &["event-1".to_string()],
            "request-1",
            "digest-1",
            "{}",
        )
        .unwrap();

        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO llm_logs (id, agent_id, session_id, prompt, response)
             VALUES ('llm-1', 'agent-1', 'session-1', '{}', '{}')",
            [],
        )
        .unwrap();
        assert!(mark_tool_completion_events_consumed_in_transaction(
            &tx,
            &["event-1".to_string()],
            "wrong-request",
        )
        .is_err());
        drop(tx);

        let log_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM llm_logs WHERE id = 'llm-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(log_count, 0);
        assert_eq!(
            list_unconsumed_tool_completion_events(&conn, "session-1").unwrap()[0].state,
            ToolCompletionState::Included
        );
    }

    #[test]
    fn included_event_survives_database_reopen() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "opencrab-issue975-restart-{}-{nonce}.db",
            std::process::id()
        ));
        {
            let conn = crate::init_connection(path.to_str().unwrap()).unwrap();
            conn.execute(
                "INSERT INTO memory_sessions
                 (id, agent_id, session_id, log_type, content, created_at)
                 VALUES (42, 'agent-1', 'session-1', 'tool_result', 'done',
                         '2026-09-08T00:00:00Z')",
                [],
            )
            .unwrap();
            enqueue_tool_completion_event(&conn, &event("event-1", "exec-1")).unwrap();
            mark_tool_completion_events_included(
                &conn,
                &["event-1".to_string()],
                "request-1",
                "digest-1",
                r#"{"model":"test"}"#,
            )
            .unwrap();
        }
        {
            let conn = crate::init_connection(path.to_str().unwrap()).unwrap();
            let rows = list_unconsumed_tool_completion_events(&conn, "session-1").unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].state, ToolCompletionState::Included);
            assert_eq!(rows[0].included_request_id.as_deref(), Some("request-1"));
            assert_eq!(rows[0].request_digest.as_deref(), Some("digest-1"));
            let request_json: String = conn
                .query_row(
                    "SELECT request_json FROM tool_completion_requests WHERE request_id = 'request-1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(request_json, r#"{"model":"test"}"#);
            mark_tool_completion_events_consumed(&conn, &["event-1".to_string()], "request-1")
                .unwrap();
            assert!(list_unconsumed_tool_completion_events(&conn, "session-1")
                .unwrap()
                .is_empty());
        }
        let _ = std::fs::remove_file(path);
    }
}
