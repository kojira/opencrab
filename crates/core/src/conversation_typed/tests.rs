use std::collections::HashSet;

use serde_json::{json, Value};

use opencrab_llm_types::Role;

use super::{
    Omission, OmissionPointer, OmissionTarget, ResultBody, ToolCallState, ToolResultState,
    TypedItem,
};

const AGENT: &str = "agent-1";
const USER: &str = "user-9";

fn row(
    id: i64,
    log_type: &str,
    speaker: Option<&str>,
    content: &str,
    meta: Option<Value>,
) -> opencrab_db::queries::SessionLogRow {
    opencrab_db::queries::SessionLogRow {
        id: Some(id),
        agent_id: AGENT.to_string(),
        session_id: "sess-1".to_string(),
        log_type: log_type.to_string(),
        content: content.to_string(),
        speaker_id: speaker.map(str::to_string),
        turn_number: None,
        metadata_json: meta.map(|value| value.to_string()),
        created_at: Some("2026-09-01T16:16:41+00:00".to_string()),
    }
}

fn completed_ids(logs: &[opencrab_db::queries::SessionLogRow]) -> HashSet<String> {
    logs.iter()
        .filter(|log| log.log_type == "tool_result" || log.log_type == "tool_cancelled")
        .filter_map(|log| {
            serde_json::from_str::<Value>(log.metadata_json.as_deref()?)
                .ok()?
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn derive(logs: &[opencrab_db::queries::SessionLogRow]) -> super::DerivedConversation {
    let refs = crate::conversation::ConversationRefs::build(logs, AGENT);
    let completed = completed_ids(logs);
    super::derive_items(logs, &refs, &completed, AGENT)
}

fn tool_calls_metadata(calls: Value) -> Value {
    json!({"tool_calls_json": serde_json::to_string(&calls).unwrap()})
}

fn call(call_id: &str, tool_name: &str, arguments: Value) -> Value {
    json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": tool_name,
            "arguments": serde_json::to_string(&arguments).unwrap(),
        }
    })
}


include!("tests/assembly_cases.rs");
include!("tests/safety_and_shadow.rs");
