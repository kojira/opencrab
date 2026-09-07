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

