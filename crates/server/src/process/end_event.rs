use opencrab_core::{context_budget::ContextBudgetError, EngineResult, ExplicitTermination};

fn insert_event(db: &opencrab_db::Db, agent_id: &str, session_id: &str, event: serde_json::Value) {
    let event_type = event["type"].as_str().unwrap_or("turn_event").to_string();
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        log_type: "system".to_string(),
        content: event.to_string(),
        speaker_id: None,
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    if let Ok(conn) = db.lock() {
        if let Err(error) = opencrab_db::queries::insert_session_log(&conn, &log) {
            tracing::error!(agent_id, session_id, event_type, %error, "failed to persist turn end event");
        }
    }
}

/// 正常な明示終了と、engine 内で判別できた資源切れを会話履歴へ保存する。
pub(super) fn persist_result(
    db: &opencrab_db::Db,
    agent_id: &str,
    session_id: &str,
    result: &anyhow::Result<EngineResult>,
) {
    let event = match result {
        Ok(engine_result) if engine_result.stopped_by_limit => Some(serde_json::json!({
            "type": "turn_exhausted",
            "reason": "iteration_limit",
            "iterations": engine_result.iterations,
        })),
        Ok(engine_result) => {
            engine_result
                .explicit_termination
                .map(|termination| match termination {
                    ExplicitTermination::NoReply => serde_json::json!({
                        "type": "turn_terminated",
                        "marker": opencrab_core::NO_REPLY_SENTINEL,
                    }),
                })
        }
        Err(error) => error.chain().find_map(|cause| {
            cause
                .downcast_ref::<ContextBudgetError>()
                .and_then(context_budget_event)
        }),
    };
    if let Some(event) = event {
        insert_event(db, agent_id, session_id, event);
    }
}

/// request envelope の構築中に尽きた予算は engine result を作れないため、その場で保存する。
pub(super) fn persist_context_budget_error(
    db: &opencrab_db::Db,
    agent_id: &str,
    session_id: &str,
    error: &ContextBudgetError,
) {
    if let Some(event) = context_budget_event(error) {
        insert_event(db, agent_id, session_id, event);
    }
}

fn context_budget_event(error: &ContextBudgetError) -> Option<serde_json::Value> {
    matches!(error, ContextBudgetError::Exhausted { .. }).then(|| {
        serde_json::json!({
            "type": "turn_exhausted",
            "reason": "context_budget",
            "detail": error.to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_result(stopped_by_limit: bool) -> EngineResult {
        EngineResult {
            response: String::new(),
            iterations: 11,
            tool_calls_made: 0,
            stopped_by_limit,
            explicit_termination: None,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: true,
            xml_fallback_parses: 0,
        }
    }

    #[test]
    fn iteration_limit_is_persisted_without_assistant_speech() {
        let db = opencrab_db::Db::memory().unwrap();
        persist_result(&db, "a", "s", &Ok(engine_result(true)));

        let logs = {
            let conn = db.lock().unwrap();
            opencrab_db::queries::list_session_logs_by_session(&conn, "s").unwrap()
        };
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].log_type, "system");
        let event: serde_json::Value = serde_json::from_str(&logs[0].content).unwrap();
        assert_eq!(event["type"], "turn_exhausted");
        assert_eq!(event["reason"], "iteration_limit");
        assert_eq!(event["iterations"], 11);
        assert!(
            logs.iter().all(|log| log.log_type != "speech"),
            "資源切れ本文を assistant speech として保存してはならない"
        );
    }
}
