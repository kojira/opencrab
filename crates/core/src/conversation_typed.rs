//! セッションログから導出する、送信経路とは独立した型付き会話 item。
//!
//! PR1 では shadow 観測と回帰固定だけに用い、既存の flat 会話文字列は変更しない。

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum TypedItem {
    UserSpeech {
        event_ref: Option<String>,
        speaker: String,
        timestamp: Option<String>,
        content: String,
        relation: Option<String>,
    },
    AssistantSpeech {
        timestamp: Option<String>,
        content: String,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        arguments: Value,
        state: ToolCallState,
        timestamp: Option<String>,
    },
    ToolResult {
        call_id: String,
        tool_name: String,
        body: ResultBody,
        state: ToolResultState,
        timestamp: Option<String>,
    },
    MachineEvent {
        kind: String,
        related_call: Option<String>,
        related_subtask: Option<String>,
        timestamp: Option<String>,
        payload: Value,
        opaque: bool,
    },
    ContextSection {
        kind: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ToolCallState {
    Pending,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ToolResultState {
    Pending,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ResultBody {
    Inline(Value),
    Omitted(Omission),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Omission {
    pub marker_kind: String,
    pub version: u32,
    pub reason: String,
    pub target: OmissionTarget,
    pub original_chars: Option<usize>,
    pub original_bytes: Option<usize>,
    pub original_tokens: Option<usize>,
    pub pointer: OmissionPointer,
    pub resolvable: bool,
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum OmissionTarget {
    Arguments,
    Result,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OmissionPointer {
    pub kind: String,
    pub path: Option<String>,
    pub id: Option<String>,
    pub field: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeriveDiagnostics {
    pub item_count: usize,
    pub unpaired_call_count: usize,
    pub opaque_event_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DerivedConversation {
    pub items: Vec<TypedItem>,
    pub diagnostics: DeriveDiagnostics,
}

#[derive(Debug, Clone)]
struct ParsedCall {
    call_id: String,
    tool_name: String,
    arguments: Value,
}

fn parse_tool_calls(log: &opencrab_db::queries::SessionLogRow) -> Vec<ParsedCall> {
    let Some(meta) = log
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    else {
        return Vec::new();
    };
    let Some(raw_calls) = meta.get("tool_calls_json").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(calls) = serde_json::from_str::<Value>(raw_calls)
        .ok()
        .and_then(|value| value.as_array().cloned())
    else {
        return Vec::new();
    };

    calls
        .into_iter()
        .filter_map(|item| {
            let call_id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("oc-{}", log.id.unwrap_or(0)));
            if let Some(function) = item.get("function") {
                let tool_name = function.get("name")?.as_str()?.to_owned();
                let arguments = match function.get("arguments") {
                    Some(Value::String(raw)) => {
                        // 未確認: provider が不正 JSON の arguments 文字列を保存する場合があるかは
                        // 実データで未確認。判断材料を捨てず、parse 不能時は文字列のまま保持する。
                        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
                    }
                    Some(value) => value.clone(),
                    None => Value::Null,
                };
                Some(ParsedCall {
                    call_id,
                    tool_name,
                    arguments,
                })
            } else {
                Some(ParsedCall {
                    call_id,
                    tool_name: item.get("name")?.as_str()?.to_owned(),
                    arguments: item.get("arguments").cloned().unwrap_or(Value::Null),
                })
            }
        })
        .collect()
}

fn result_metadata(log: &opencrab_db::queries::SessionLogRow) -> (Option<String>, Option<String>) {
    let meta = log
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let call_id = meta
        .as_ref()
        .and_then(|value| value.get("tool_call_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let tool_name = meta
        .as_ref()
        .and_then(|value| value.get("tool_name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    (call_id, tool_name)
}

fn value_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

fn omission(
    reason: &str,
    original: &str,
    pointer: OmissionPointer,
    resolvable: bool,
    tool: Option<String>,
) -> ResultBody {
    ResultBody::Omitted(Omission {
        marker_kind: "opencrab_omission".to_owned(),
        version: 1,
        reason: reason.to_owned(),
        target: OmissionTarget::Result,
        original_chars: Some(original.chars().count()),
        original_bytes: Some(original.len()),
        original_tokens: Some(crate::tokens::estimate_tokens(original)),
        pointer,
        resolvable,
        tool,
    })
}

/// 既存の結果参照と同じ失敗・読み・一覧・shell の分岐を、文字列でなく構造へ写す。
pub(crate) fn classify_result_body(tool_name: &str, result_json: &str) -> ResultBody {
    let value: Value = match serde_json::from_str(result_json) {
        Ok(value) => value,
        Err(_) => return ResultBody::Inline(Value::String(result_json.to_owned())),
    };
    let null = Value::Null;
    let data = value.get("data").unwrap_or(&null);

    if crate::conversation::signals_failure(&value, data) {
        return ResultBody::Inline(value);
    }
    if data.get("exit_code").is_some() {
        return ResultBody::Inline(data.clone());
    }
    if data.get("entries").and_then(Value::as_array).is_some() {
        let path = data.get("path").and_then(Value::as_str).map(str::to_owned);
        return omission(
            "list_result_budget",
            result_json,
            OmissionPointer {
                kind: "path".to_owned(),
                path,
                id: None,
                field: None,
            },
            true,
            Some(tool_name.to_owned()),
        );
    }
    if let (Some(path), Some(content)) = (
        data.get("path").and_then(Value::as_str),
        data.get("content").and_then(Value::as_str),
    ) {
        let mut body = omission(
            "read_result_budget",
            content,
            OmissionPointer {
                kind: "workspace_path".to_owned(),
                path: Some(path.to_owned()),
                id: None,
                field: None,
            },
            true,
            Some(tool_name.to_owned()),
        );
        if let ResultBody::Omitted(ref mut omitted) = body {
            omitted.original_tokens = data
                .get("estimated_tokens")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .or(omitted.original_tokens);
        }
        return body;
    }

    // 小さい結果を Inline に残す境界は、現行 flat 経路の「参照が本文より短いときだけ畳む」
    // という長さ不変条件を共有する。分岐を独自の固定 byte 値へ複製しない。
    if crate::conversation::result_reference(tool_name, result_json) == result_json {
        return ResultBody::Inline(value);
    }
    let path = data.get("path").and_then(Value::as_str).map(str::to_owned);
    omission(
        if path.is_some() {
            "mutation_result"
        } else {
            "oversized"
        },
        result_json,
        OmissionPointer {
            kind: path
                .as_ref()
                .map_or_else(|| "unavailable".to_owned(), |_| "workspace_path".to_owned()),
            path,
            id: None,
            field: None,
        },
        false,
        None,
    )
}

fn classify_log_result(
    tool_name: &str,
    result_json: &str,
    log: &opencrab_db::queries::SessionLogRow,
    field: &str,
) -> ResultBody {
    let mut body = classify_result_body(tool_name, result_json);
    if let ResultBody::Omitted(ref mut omitted) = body {
        if omitted.pointer.kind == "unavailable" {
            omitted.pointer.kind = "memory_session".to_owned();
            omitted.pointer.id = log.id.map(|id| id.to_string());
            omitted.pointer.field = Some(field.to_owned());
        }
    }
    body
}

fn opaque_event(
    log: &opencrab_db::queries::SessionLogRow,
    kind: String,
    related_call: Option<String>,
    related_subtask: Option<String>,
    payload: Value,
) -> TypedItem {
    TypedItem::MachineEvent {
        kind,
        related_call,
        related_subtask,
        timestamp: log.created_at.clone(),
        payload,
        opaque: true,
    }
}

/// retain 済みの同じログ列から型付き item を決定的に導出する。
pub(crate) fn derive_items(
    logs: &[opencrab_db::queries::SessionLogRow],
    refs: &crate::conversation::ConversationRefs,
    completed_ids: &HashSet<String>,
    agent_id: &str,
) -> DerivedConversation {
    let parsed_by_log: Vec<Vec<ParsedCall>> = logs.iter().map(parse_tool_calls).collect();
    let mut call_names = HashMap::new();
    for call in parsed_by_log.iter().flatten() {
        call_names.insert(call.call_id.clone(), call.tool_name.clone());
    }

    // 1 パス目は記録済み ID だけを収集する。時刻・tool 名・近傍では相関しない。
    let mut subtask_to_call = HashMap::new();
    let mut seen_spawn_ids = HashSet::new();
    for log in logs {
        let Some(subtask_id) = crate::conversation::spawn_ack_subtask_id(log) else {
            continue;
        };
        if !seen_spawn_ids.insert(subtask_id.clone()) {
            continue;
        }
        let (call_id, _) = result_metadata(log);
        if let Some(call_id) = call_id.filter(|id| call_names.contains_key(id)) {
            subtask_to_call.insert(subtask_id, call_id);
        }
    }

    let mut items = Vec::new();
    let mut call_item_indices: HashMap<String, Vec<usize>> = HashMap::new();
    let mut pending_spawn_items: HashMap<String, usize> = HashMap::new();
    let mut result_seen_calls = HashSet::new();
    let mut cancelled_calls = HashSet::new();
    let mut opaque_event_count = 0usize;
    let mut unpaired_call_count = 0usize;
    let mut emitted_spawn_ids = HashSet::new();

    for (log_index, log) in logs.iter().enumerate() {
        match log.log_type.as_str() {
            "speech" => {
                if log.speaker_id.as_deref() == Some(agent_id) {
                    items.push(TypedItem::AssistantSpeech {
                        timestamp: log.created_at.clone(),
                        content: log.content.clone(),
                    });
                } else {
                    items.push(TypedItem::UserSpeech {
                        event_ref: refs.event_of(log).map(|n| format!("e{n}")),
                        speaker: log
                            .speaker_id
                            .clone()
                            .unwrap_or_else(|| log.agent_id.clone()),
                        timestamp: log.created_at.clone(),
                        content: log.content.clone(),
                        relation: None,
                    });
                }
            }
            "tool_call" => {
                let calls = &parsed_by_log[log_index];
                if calls.is_empty() {
                    opaque_event_count += 1;
                    items.push(opaque_event(
                        log,
                        "tool_call".to_owned(),
                        None,
                        None,
                        value_or_string(&log.content),
                    ));
                    continue;
                }
                for call in calls {
                    let index = items.len();
                    call_item_indices
                        .entry(call.call_id.clone())
                        .or_default()
                        .push(index);
                    items.push(TypedItem::ToolCall {
                        call_id: call.call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        arguments: call.arguments.clone(),
                        state: if completed_ids.contains(&call.call_id) {
                            ToolCallState::Completed
                        } else {
                            ToolCallState::Pending
                        },
                        timestamp: log.created_at.clone(),
                    });
                }
            }
            "tool_result" => {
                let (call_id, metadata_tool_name) = result_metadata(log);
                let related_subtask = crate::conversation::spawn_ack_subtask_id(log);
                let Some(call_id) = call_id else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(
                        log,
                        "tool_result".to_owned(),
                        None,
                        related_subtask,
                        value_or_string(&log.content),
                    ));
                    continue;
                };
                let Some(call_tool_name) = call_names.get(&call_id) else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(
                        log,
                        "tool_result".to_owned(),
                        Some(call_id),
                        related_subtask,
                        value_or_string(&log.content),
                    ));
                    continue;
                };
                let tool_name = metadata_tool_name.unwrap_or_else(|| call_tool_name.clone());
                if let Some(subtask_id) = related_subtask {
                    if !emitted_spawn_ids.insert(subtask_id.clone()) {
                        continue;
                    }
                    result_seen_calls.insert(call_id.clone());
                    let index = items.len();
                    items.push(TypedItem::ToolResult {
                        call_id,
                        tool_name,
                        body: ResultBody::Inline(value_or_string(&log.content)),
                        state: ToolResultState::Pending,
                        timestamp: log.created_at.clone(),
                    });
                    pending_spawn_items.insert(subtask_id, index);
                } else {
                    result_seen_calls.insert(call_id.clone());
                    let body = classify_log_result(&tool_name, &log.content, log, "content");
                    items.push(TypedItem::ToolResult {
                        call_id,
                        tool_name,
                        body,
                        state: ToolResultState::Completed,
                        timestamp: log.created_at.clone(),
                    });
                }
            }
            "tool_cancelled" => {
                let (call_id, metadata_tool_name) = result_metadata(log);
                let Some(call_id) = call_id else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(
                        log,
                        "tool_cancelled".to_owned(),
                        None,
                        None,
                        value_or_string(&log.content),
                    ));
                    continue;
                };
                let Some(call_tool_name) = call_names.get(&call_id) else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(
                        log,
                        "tool_cancelled".to_owned(),
                        Some(call_id),
                        None,
                        value_or_string(&log.content),
                    ));
                    continue;
                };
                let tool_name = metadata_tool_name.unwrap_or_else(|| call_tool_name.clone());
                result_seen_calls.insert(call_id.clone());
                cancelled_calls.insert(call_id.clone());
                items.push(TypedItem::ToolResult {
                    call_id,
                    tool_name,
                    body: ResultBody::Inline(value_or_string(&log.content)),
                    state: ToolResultState::Cancelled,
                    timestamp: log.created_at.clone(),
                });
            }
            "system" => {
                let Ok(payload) = serde_json::from_str::<Value>(&log.content) else {
                    opaque_event_count += 1;
                    items.push(opaque_event(
                        log,
                        "system".to_owned(),
                        None,
                        None,
                        Value::String(log.content.clone()),
                    ));
                    continue;
                };
                let Some(kind) = payload.get("type").and_then(Value::as_str) else {
                    opaque_event_count += 1;
                    items.push(opaque_event(log, "system".to_owned(), None, None, payload));
                    continue;
                };
                if kind != "subtask_completed" {
                    items.push(TypedItem::MachineEvent {
                        kind: kind.to_owned(),
                        related_call: None,
                        related_subtask: payload
                            .get("subtask_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        timestamp: log.created_at.clone(),
                        payload,
                        opaque: false,
                    });
                    continue;
                }
                let subtask_id = payload
                    .get("subtask_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                let Some(subtask_id) = subtask_id else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(log, kind.to_owned(), None, None, payload));
                    continue;
                };
                let Some(call_id) = subtask_to_call.get(&subtask_id).cloned() else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(
                        log,
                        kind.to_owned(),
                        None,
                        Some(subtask_id),
                        payload,
                    ));
                    continue;
                };
                let Some(tool_name) = call_names.get(&call_id).cloned() else {
                    opaque_event_count += 1;
                    unpaired_call_count += 1;
                    items.push(opaque_event(
                        log,
                        kind.to_owned(),
                        Some(call_id),
                        Some(subtask_id),
                        payload,
                    ));
                    continue;
                };
                let body = match payload.get("result") {
                    Some(Value::String(result)) => {
                        classify_log_result(&tool_name, result, log, "content.result")
                    }
                    Some(result) => ResultBody::Inline(result.clone()),
                    None => ResultBody::Inline(payload.clone()),
                };
                result_seen_calls.insert(call_id.clone());
                let final_result = TypedItem::ToolResult {
                    call_id,
                    tool_name,
                    body,
                    state: ToolResultState::Completed,
                    timestamp: log.created_at.clone(),
                };
                if let Some(index) = pending_spawn_items.remove(&subtask_id) {
                    items[index] = final_result;
                } else {
                    items.push(final_result);
                }
            }
            other => {
                opaque_event_count += 1;
                items.push(opaque_event(
                    log,
                    other.to_owned(),
                    None,
                    None,
                    Value::String(log.content.clone()),
                ));
            }
        }
    }

    for call_id in call_names.keys() {
        if !result_seen_calls.contains(call_id) {
            unpaired_call_count += 1;
        }
    }
    for call_id in cancelled_calls {
        if let Some(indices) = call_item_indices.get(&call_id) {
            for &index in indices {
                if let TypedItem::ToolCall { state, .. } = &mut items[index] {
                    *state = ToolCallState::Cancelled;
                }
            }
        }
    }

    let diagnostics = DeriveDiagnostics {
        item_count: items.len(),
        unpaired_call_count,
        opaque_event_count,
    };
    DerivedConversation { items, diagnostics }
}

#[cfg(test)]
pub(crate) fn run_shadow_comparison(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    conversation_low: usize,
    include_memory_index: bool,
) -> DeriveDiagnostics {
    let flat_tokens = match crate::conversation::build_conversation_string_with_waters(
        conn,
        session_id,
        agent_id,
        conversation_high,
        conversation_low,
        include_memory_index,
    ) {
        Ok(flat) => crate::tokens::estimate_tokens(&flat),
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not build flat conversation");
            0
        }
    };
    let snapshot = match opencrab_db::queries::latest_conversation_snapshot(conn, session_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not read snapshot");
            return DeriveDiagnostics {
                item_count: 0,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            };
        }
    };
    let logs = match snapshot {
        Some(snapshot) => {
            opencrab_db::queries::list_session_logs_after(conn, session_id, snapshot.through_log_id)
        }
        None => opencrab_db::queries::list_session_logs_by_session(conn, session_id),
    };
    let logs = match logs {
        Ok(logs) => crate::conversation::retain_conversation_logs(logs),
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not read conversation logs");
            return DeriveDiagnostics {
                item_count: 0,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            };
        }
    };
    let all = match opencrab_db::queries::list_session_logs_by_session(conn, session_id) {
        Ok(logs) => crate::conversation::retain_conversation_logs(logs),
        Err(error) => {
            tracing::warn!(session_id, %error, "typed shadow could not read full conversation logs");
            return DeriveDiagnostics {
                item_count: 0,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            };
        }
    };
    let refs = crate::conversation::ConversationRefs::build(&all, agent_id);
    let completed = all
        .iter()
        .filter(|log| log.log_type == "tool_result" || log.log_type == "tool_cancelled")
        .filter_map(|log| result_metadata(log).0)
        .collect();
    let derived = derive_items(&logs, &refs, &completed, agent_id);
    let typed_json = serde_json::to_string(&derived.items).unwrap_or_else(|error| {
        tracing::warn!(session_id, %error, "typed shadow could not serialize items");
        String::new()
    });
    // 未確認: provider 別 item overhead は PR2 以降で実測する。ここは JSON 近似値だけを比較する。
    let typed_tokens = crate::tokens::estimate_tokens(&typed_json);
    tracing::debug!(
        session_id,
        typed_items = derived.diagnostics.item_count,
        unpaired = derived.diagnostics.unpaired_call_count,
        opaque = derived.diagnostics.opaque_event_count,
        typed_tokens,
        flat_tokens,
        "typed shadow comparison"
    );
    derived.diagnostics
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::{json, Value};

    use super::{ResultBody, ToolCallState, ToolResultState, TypedItem};

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

    fn sleep_rows(include_settle: bool) -> Vec<opencrab_db::queries::SessionLogRow> {
        let mut logs = vec![
            row(99, "speech", Some(USER), "sleep を実行して", None),
            row(
                100,
                "tool_call",
                Some(AGENT),
                "execute_shell",
                Some(tool_calls_metadata(json!([call(
                    "call_c253",
                    "execute_shell",
                    json!({
                        "args": ["60"],
                        "command": "sleep",
                        "stdin": "",
                        "timeout_secs": 90,
                    }),
                )]))),
            ),
            row(
                101,
                "tool_result",
                Some(AGENT),
                r#"{"status":"spawned","subtask_id":"s76","tool":"execute_shell","tool_call_id":"call_c253"}"#,
                Some(json!({
                    "tool_call_id": "call_c253",
                    "tool_name": "execute_shell",
                })),
            ),
        ];
        if include_settle {
            logs.push(row(
                102,
                "system",
                None,
                &json!({
                    "type": "subtask_completed",
                    "subtask_id": "s76",
                    "session_id": "sub-1",
                    "exit_reason": "completed",
                    "result": serde_json::to_string(&json!({
                        "success": true,
                        "data": {"exit_code": 0, "stdout": "", "stderr": ""},
                    }))
                    .unwrap(),
                })
                .to_string(),
                None,
            ));
        }
        logs
    }

    // settle 前でも実行引数を構造のまま保持し、spawn 受理だけを Pending 結果として示す。
    #[test]
    fn args_visible_before_settle() {
        let derived = derive(&sleep_rows(false));
        let call = derived.items.iter().find(|item| {
            matches!(
                item,
                TypedItem::ToolCall { call_id, tool_name, .. }
                    if call_id == "call_c253" && tool_name == "execute_shell"
            )
        });
        let Some(TypedItem::ToolCall { arguments, .. }) = call else {
            panic!("sleep の ToolCall が存在すること");
        };
        assert_eq!(arguments["command"], "sleep");
        assert_eq!(arguments["args"], json!(["60"]));
        assert_eq!(arguments["timeout_secs"], 90);
        assert!(!arguments.to_string().contains("→log:"));
        assert!(derived.items.iter().any(|item| {
            matches!(
                item,
                TypedItem::ToolResult {
                    call_id,
                    state: ToolResultState::Pending,
                    ..
                } if call_id == "call_c253"
            )
        }));
    }

    // settle 後も元引数を失わず、spawn 受理を二重化せず正確な完了結果へ置換する。
    #[test]
    fn args_visible_and_result_after_settle() {
        let derived = derive(&sleep_rows(true));
        let call = derived.items.iter().find(
            |item| matches!(item, TypedItem::ToolCall { call_id, .. } if call_id == "call_c253"),
        );
        let Some(TypedItem::ToolCall { arguments, .. }) = call else {
            panic!("sleep の ToolCall が存在すること");
        };
        assert_eq!(arguments["command"], "sleep");
        assert_eq!(arguments["args"], json!(["60"]));
        assert_eq!(arguments["timeout_secs"], 90);
        assert!(!arguments.to_string().contains("→log:"));

        let results: Vec<_> = derived
            .items
            .iter()
            .filter(|item| matches!(item, TypedItem::ToolResult { .. }))
            .collect();
        assert_eq!(results.len(), 1);
        let TypedItem::ToolResult {
            call_id,
            body,
            state,
            ..
        } = results[0]
        else {
            unreachable!();
        };
        assert_eq!(call_id, "call_c253");
        assert_eq!(*state, ToolResultState::Completed);
        let ResultBody::Inline(value) = body else {
            panic!("shell 完了結果は Inline であること");
        };
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["stdout"], "");
    }

    // 機械文字列を含むユーザ本文も型を偽装せず、原文の UserSpeech として保持する。
    #[test]
    fn user_speech_stays_user_even_with_machine_strings() {
        let content = "本文は会話に残していない\n→ subtask s76 を起動\n[tool_result]\n\u{1}";
        let derived = derive(&[row(1, "speech", Some(USER), content, None)]);
        assert!(matches!(
            derived.items.as_slice(),
            [TypedItem::UserSpeech {
                content: actual,
                ..
            }] if actual == content
        ));
        assert!(!derived.items.iter().any(|item| matches!(
            item,
            TypedItem::ToolResult { .. } | TypedItem::MachineEvent { .. }
        )));
    }

    // 結果は記録済み call_id だけへ結び、不明 ID は opaque、取消は call と result 双方へ反映する。
    #[test]
    fn results_bind_only_to_recorded_call_ids() {
        let batch = derive(&[
            row(
                1,
                "tool_call",
                Some(AGENT),
                "batch",
                Some(tool_calls_metadata(json!([
                    call("call_a", "tool_a", json!({"slot": "a"})),
                    call("call_b", "tool_b", json!({"slot": "b"})),
                ]))),
            ),
            row(
                2,
                "tool_result",
                Some(AGENT),
                r#"{"success":true,"data":{"exit_code":0,"stdout":"a","stderr":""}}"#,
                Some(json!({"tool_call_id": "call_a", "tool_name": "tool_a"})),
            ),
            row(
                3,
                "tool_result",
                Some(AGENT),
                r#"{"success":true,"data":{"exit_code":0,"stdout":"b","stderr":""}}"#,
                Some(json!({"tool_call_id": "call_b", "tool_name": "tool_b"})),
            ),
        ]);
        let result_a = batch.items.iter().find(
            |item| matches!(item, TypedItem::ToolResult { call_id, .. } if call_id == "call_a"),
        );
        assert!(matches!(
            result_a,
            Some(TypedItem::ToolResult {
                tool_name,
                body: ResultBody::Inline(value),
                ..
            }) if tool_name == "tool_a" && value["stdout"] == "a"
        ));
        let result_b = batch.items.iter().find(
            |item| matches!(item, TypedItem::ToolResult { call_id, .. } if call_id == "call_b"),
        );
        assert!(matches!(
            result_b,
            Some(TypedItem::ToolResult {
                tool_name,
                body: ResultBody::Inline(value),
                ..
            }) if tool_name == "tool_b" && value["stdout"] == "b"
        ));

        let unknown = derive(&[
            row(
                10,
                "tool_call",
                Some(AGENT),
                "tool_x",
                Some(tool_calls_metadata(json!([call(
                    "call_x",
                    "tool_x",
                    json!({}),
                )]))),
            ),
            row(
                11,
                "tool_result",
                Some(AGENT),
                r#"{"success":true}"#,
                Some(json!({
                    "tool_call_id": "call_UNKNOWN",
                    "tool_name": "tool_x",
                })),
            ),
        ]);
        assert!(unknown.items.iter().any(|item| matches!(
            item,
            TypedItem::MachineEvent {
                kind,
                related_call: Some(call_id),
                opaque: true,
                ..
            } if kind == "tool_result" && call_id == "call_UNKNOWN"
        )));
        assert!(unknown.diagnostics.opaque_event_count >= 1);
        assert!(unknown.diagnostics.unpaired_call_count >= 1);

        let cancelled = derive(&[
            row(
                20,
                "tool_call",
                Some(AGENT),
                "tool_c",
                Some(tool_calls_metadata(json!([call(
                    "call_c",
                    "tool_c",
                    json!({}),
                )]))),
            ),
            row(
                21,
                "tool_cancelled",
                Some(AGENT),
                r#"{"reason":"cancelled"}"#,
                Some(json!({"tool_call_id": "call_c", "tool_name": "tool_c"})),
            ),
        ]);
        assert!(cancelled.items.iter().any(|item| matches!(
            item,
            TypedItem::ToolCall {
                call_id,
                state: ToolCallState::Cancelled,
                ..
            } if call_id == "call_c"
        )));
        assert!(cancelled.items.iter().any(|item| matches!(
            item,
            TypedItem::ToolResult {
                call_id,
                state: ToolResultState::Cancelled,
                ..
            } if call_id == "call_c"
        )));
    }

    // 環境変数だけに存在する秘密値を typed item が暗黙に取り込まないことを固定する。
    #[test]
    fn env_injected_secret_never_in_items() {
        const SECRET: &str = "S3CRET-DO-NOT-LEAK-abcdef";
        std::env::set_var("OC_TEST_SECRET", SECRET);
        let serialized = serde_json::to_string(&derive(&sleep_rows(true)).items).unwrap();
        let leaked = serialized.contains(SECRET);
        std::env::remove_var("OC_TEST_SECRET");
        assert!(!leaked);
    }

    // 大きな read 結果だけを説明文なしの構造化 omission にし、shell stdout は Inline に残す。
    #[test]
    fn large_read_result_becomes_structured_omission() {
        let large_content = "x".repeat(80_000);
        let derived = derive(&[
            row(
                1,
                "tool_call",
                Some(AGENT),
                "ws_read",
                Some(tool_calls_metadata(json!([call(
                    "call_r",
                    "ws_read",
                    json!({"path": "/workspace/report.txt"}),
                )]))),
            ),
            row(
                2,
                "tool_result",
                Some(AGENT),
                &json!({
                    "success": true,
                    "data": {
                        "path": "/workspace/report.txt",
                        "content": large_content,
                        "has_more": true,
                    },
                })
                .to_string(),
                Some(json!({"tool_call_id": "call_r", "tool_name": "ws_read"})),
            ),
            row(
                3,
                "tool_call",
                Some(AGENT),
                "execute_shell",
                Some(tool_calls_metadata(json!([call(
                    "call_s",
                    "execute_shell",
                    json!({"command": "printf", "args": ["ok"]}),
                )]))),
            ),
            row(
                4,
                "tool_result",
                Some(AGENT),
                r#"{"success":true,"data":{"exit_code":0,"stdout":"ok","stderr":""}}"#,
                Some(json!({
                    "tool_call_id": "call_s",
                    "tool_name": "execute_shell",
                })),
            ),
        ]);
        let read_result = derived.items.iter().find(
            |item| matches!(item, TypedItem::ToolResult { call_id, .. } if call_id == "call_r"),
        );
        let Some(TypedItem::ToolResult { body, .. }) = read_result else {
            panic!("ws_read の ToolResult が存在すること");
        };
        let ResultBody::Omitted(omitted) = body else {
            panic!("大きな read 結果は Omitted であること");
        };
        assert_eq!(omitted.reason, "read_result_budget");
        assert_eq!(omitted.pointer.kind, "workspace_path");
        assert_eq!(
            omitted.pointer.path.as_deref(),
            Some("/workspace/report.txt")
        );
        assert!(omitted.resolvable);
        assert_eq!(omitted.tool.as_deref(), Some("ws_read"));
        let omission_json = serde_json::to_string(omitted).unwrap();
        assert!(!omission_json.contains("本文は会話に残していない"));
        assert!(!omission_json.contains("必要ならもう一度"));

        assert!(derived.items.iter().any(|item| matches!(
            item,
            TypedItem::ToolResult {
                call_id,
                body: ResultBody::Inline(value),
                ..
            } if call_id == "call_s" && value["stdout"] == "ok"
        )));
    }

    // DB 上の snapshot なしログ列でも shadow builder が最後まで疎通することを固定する。
    #[test]
    fn shadow_comparison_runs_over_db() {
        let conn = opencrab_db::init_memory().unwrap();
        for mut log in sleep_rows(true) {
            log.id = None;
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
        }
        let diagnostics =
            super::run_shadow_comparison(&conn, "sess-1", AGENT, 100_000, 50_000, false);
        assert!(diagnostics.item_count > 0);
    }
}
