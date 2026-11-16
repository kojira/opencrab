use opencrab_core::{
    context_budget::ContextBudgetError, EngineResult, ExplicitTermination, LlmCallFailed,
};

fn insert_event(
    writer: &super::TurnLogWriter,
    agent_id: &str,
    session_id: &str,
    event: serde_json::Value,
) {
    let end = opencrab_core::history::Event::System {
        content: event.to_string(),
        speaker_id: None,
        metadata: None,
    };
    writer.write(end.row(agent_id, session_id), "turn end event");
}

/// 正常な明示終了と、engine 内で判別できた資源切れを会話履歴へ保存する。
pub(super) fn persist_result(
    writer: &super::TurnLogWriter,
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
                .or_else(|| {
                    cause
                        .downcast_ref::<LlmCallFailed>()
                        .map(llm_call_failed_event)
                })
        }),
    };
    if let Some(event) = event {
        insert_event(writer, agent_id, session_id, event);
    }
}

/// request envelope の構築中に尽きた予算は engine result を作れないため、その場で保存する。
pub(super) fn persist_context_budget_error(
    writer: &super::TurnLogWriter,
    agent_id: &str,
    session_id: &str,
    error: &ContextBudgetError,
) {
    if let Some(event) = context_budget_event(error) {
        insert_event(writer, agent_id, session_id, event);
    }
}

/// D-1131: a failed LLM call ended the turn. The agent sees in its next turn which model failed
/// and what the provider said, e.g. after a `select_llm` switch to a model the provider rejects.
fn llm_call_failed_event(failure: &LlmCallFailed) -> serde_json::Value {
    serde_json::json!({
        "type": "turn_failed",
        "model": failure.model,
        "error_code": failure.code,
        "detail": opencrab_core::llm_text::truncate_chars(&failure.body, LLM_FAILURE_DETAIL_CHARS),
    })
}

/// The provider error text kept in a `turn_failed` event. Router errors are one short line per
/// provider tried; the cut only bounds an unusually long provider body.
const LLM_FAILURE_DETAIL_CHARS: usize = 500;

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
            silent_origins: Vec::new(),
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: true,
            xml_fallback_parses: 0,
        }
    }

    /// D-1131: a turn that ended on a failed LLM call (e.g. the model `select_llm` switched to was
    /// rejected) leaves a `turn_failed` event naming the model and the provider error.
    #[test]
    fn failed_llm_call_is_persisted_as_turn_failed() {
        let db = opencrab_db::Db::memory().unwrap();
        let body = "All providers failed for model 'opus-5.5' (1 tried):\n  [chatgpt] ChatGPT API error (400): model_not_found";
        let error = anyhow::Error::from(LlmCallFailed {
            model: "opus-5.5".to_string(),
            code: "error".to_string(),
            body: body.to_string(),
        });
        persist_result(
            &crate::process::TurnLogWriter::direct(db.clone()),
            "a",
            "s",
            &Err(error),
        );

        let logs = {
            let conn = db.lock().unwrap();
            opencrab_db::queries::list_session_logs_by_session(&conn, "s").unwrap()
        };
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].log_type, "system");
        let event: serde_json::Value = serde_json::from_str(&logs[0].content).unwrap();
        assert_eq!(event["type"], "turn_failed");
        assert_eq!(event["model"], "opus-5.5");
        assert!(event["detail"]
            .as_str()
            .unwrap()
            .contains("model_not_found"));

        let rendered =
            opencrab_core::conversation::format_single_log_with_echo(&logs[0], None, None);
        assert!(rendered.contains("opus-5.5"), "{rendered}");
        assert!(rendered.contains("model_not_found"), "{rendered}");
    }

    #[test]
    fn iteration_limit_is_persisted_without_assistant_speech() {
        let db = opencrab_db::Db::memory().unwrap();
        persist_result(
            &crate::process::TurnLogWriter::direct(db.clone()),
            "a",
            "s",
            &Ok(engine_result(true)),
        );

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
