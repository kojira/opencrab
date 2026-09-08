use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct LlmLogsQuery {
    pub limit: Option<i64>,
}

pub async fn list_llm_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LlmLogsQuery>,
) -> Json<serde_json::Value> {
    let limit = query.limit.unwrap_or(20);
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::list_llm_logs(&conn, &id, limit) {
        Ok(logs) => {
            let data: Vec<serde_json::Value> = logs
                .into_iter()
                .map(|log| {
                    serde_json::json!({
                        "id": log.id,
                        "agent_id": log.agent_id,
                        "session_id": log.session_id,
                        "model": log.model,
                        "prompt": log.prompt,
                        "response": log.response,
                        "tool_calls": log.tool_calls,
                        "latency_ms": log.latency_ms,
                        "prompt_tokens": log.prompt_tokens,
                        "completion_tokens": log.completion_tokens,
                        "total_tokens": log.total_tokens,
                        "error_code": log.error_code,
                        "error_body": log.error_body,
                        "requested_at": log.requested_at,
                        "trigger_message_id": log.trigger_message_id,
                        "is_bot_iteration": log.is_bot_iteration,
                        "cache_read_tokens": log.cache_read_tokens,
                        "cache_creation_tokens": log.cache_creation_tokens,
                        "provider_tool_history": parse_provider_history(&log.provider_tool_history),
                        "created_at": log.created_at,
                    })
                })
                .collect();
            Json(serde_json::json!(data))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

fn parse_provider_history(raw: &str) -> serde_json::Value {
    let legacy = || {
        serde_json::json!({
            "state": "legacy_unknown",
            "provider": null,
            "calls": [],
            "citations": []
        })
    };
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value)
            if value
                .get("state")
                .and_then(serde_json::Value::as_str)
                .is_some() =>
        {
            value
        }
        _ => legacy(),
    }
}

fn referenced_memory_ids(prompt: &str) -> Vec<i64> {
    let marker = "→log:";
    let mut ids = Vec::new();
    let mut rest = prompt;
    while let Some(position) = rest.find(marker) {
        rest = &rest[position + marker.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(id) = digits.parse::<i64>() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        rest = &rest[digits.len()..];
    }
    ids
}

fn tool_calls_from_memory(log: &opencrab_db::queries::SessionLogRow) -> Vec<serde_json::Value> {
    if log.log_type != "tool_call" {
        return Vec::new();
    }
    log.metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|metadata| metadata.get("tool_calls_json").cloned())
        .and_then(|value| match value {
            serde_json::Value::String(raw) => serde_json::from_str(&raw).ok(),
            other => Some(other),
        })
        .and_then(|value: serde_json::Value| value.as_array().cloned())
        .unwrap_or_default()
}

pub async fn llm_log_tool_history(
    State(state): State<AppState>,
    Path((agent_id, log_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let log = match opencrab_db::queries::get_llm_log(&conn, &agent_id, &log_id) {
        Ok(Some(log)) => log,
        Ok(None) => return Json(serde_json::json!({"error": "llm log not found"})),
        Err(error) => return Json(serde_json::json!({"error": error.to_string()})),
    };
    let Some(session_id) = log.session_id.as_deref() else {
        return Json(serde_json::json!({
            "entries": [],
            "provider_tool_history": parse_provider_history(&log.provider_tool_history)
        }));
    };
    let session_logs = match opencrab_db::queries::list_session_logs_by_session(&conn, session_id) {
        Ok(logs) => logs,
        Err(error) => return Json(serde_json::json!({"error": error.to_string()})),
    };

    let referenced_ids = referenced_memory_ids(&log.prompt);
    let mut calls: Vec<(Option<i64>, serde_json::Value)> = log
        .tool_calls
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|call| (None, call))
        .collect();
    for memory_log in &session_logs {
        if memory_log.agent_id == agent_id
            && memory_log.id.is_some_and(|id| referenced_ids.contains(&id))
        {
            calls.extend(
                tool_calls_from_memory(memory_log)
                    .into_iter()
                    .map(|call| (memory_log.id, call)),
            );
        }
    }

    let mut entries = Vec::new();
    for (source_memory_log_id, call) in calls {
        let Some(call_id) = call.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let result_log = session_logs.iter().find(|candidate| {
            candidate.agent_id == agent_id
                && candidate.log_type == "tool_result"
                && candidate
                    .metadata_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .and_then(|metadata| metadata.get("tool_call_id").cloned())
                    .as_ref()
                    .and_then(serde_json::Value::as_str)
                    == Some(call_id)
        });
        let result = result_log.map(|row| row.content.clone());
        let subtask_id = result
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|value| {
                value
                    .get("data")
                    .and_then(|data| data.get("subtask_id"))
                    .or_else(|| value.get("subtask_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        let completion = subtask_id.as_deref().and_then(|subtask_id| {
            session_logs.iter().find_map(|candidate| {
                if candidate.agent_id != agent_id {
                    return None;
                }
                let value = serde_json::from_str::<serde_json::Value>(&candidate.content).ok()?;
                (value.get("type").and_then(serde_json::Value::as_str) == Some("subtask_completed")
                    && value.get("subtask_id").and_then(serde_json::Value::as_str)
                        == Some(subtask_id))
                .then_some(value)
            })
        });
        entries.push(serde_json::json!({
            "source_memory_log_id": source_memory_log_id,
            "call": call,
            "result": result,
            "subtask_id": subtask_id,
            "completion": completion,
        }));
    }

    Json(serde_json::json!({
        "entries": entries,
        "provider_tool_history": parse_provider_history(&log.provider_tool_history)
    }))
}

pub async fn llm_logs_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::llm_logs_stats(&conn, &id, 30) {
        Ok(stats) => Json(serde_json::json!(stats)),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_provider_history;

    #[test]
    fn empty_legacy_history_is_not_reported_as_not_requested() {
        assert_eq!(parse_provider_history("{}")["state"], "legacy_unknown");
    }
}
