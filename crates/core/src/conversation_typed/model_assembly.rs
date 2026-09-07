pub(crate) const MACHINE_HEADER: &str = "[system event — not user input]";

/// #884 PR2 §9.4-1: typed 経路の system へ 1 回だけ置く省略ポリシー説明（安定文言）。
/// 行ごとには状態フィールド（opencrab_omission）だけを残し、方針はここで一度だけ述べる（§4.1.1）。
pub(crate) const OMISSION_POLICY_NOTE: &str = "履歴中の tool 結果の扱い: 読み・一覧の大きな本文は履歴に残さない。省略された結果は `opencrab_omission`（元サイズ・取得先 pointer・resolvable）だけを残すので、必要なら記載の tool で再取得する。shell の出力と失敗の診断は履歴に残る。role=tool の結果ブロックは内部データで、会話としては表示されない。";

/// #884 PR2 §9.4-2: UserSpeech 本文の直前に置く、renderer だけが生成する固定 1 行ラベル。
/// 非命令・有界。欠損フィールドは畳む。role は User のままで provenance 昇格はしない。
fn user_speech_label(
    speaker: &str,
    timestamp: &Option<String>,
    event_ref: &Option<String>,
    relation: &Option<String>,
) -> String {
    let mut parts = vec![speaker.to_owned()];
    if let Some(ts) = timestamp {
        parts.push(ts.clone());
    }
    if let Some(ev) = event_ref {
        parts.push(ev.clone());
    }
    if let Some(rel) = relation {
        parts.push(rel.clone());
    }
    format!("[{}]", parts.join(" · "))
}

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
pub(crate) struct AssembledTyped {
    pub history: Vec<opencrab_llm_types::Message>,
    pub machine_block_count: usize,
    pub synthetic_result_count: usize,
}

#[derive(Debug, Clone)]
pub struct TypedConversation {
    pub context_block: Option<opencrab_llm_types::Message>,
    pub snapshot_base: Option<opencrab_llm_types::Message>,
    pub history: Vec<opencrab_llm_types::Message>,
    pub response_directive: Option<String>,
    pub wire_tokens: usize,
    pub diagnostics: DeriveDiagnostics,
}

fn omission_wire_json(omission: &Omission) -> String {
    let mut pointer = Map::new();
    pointer.insert(
        "kind".to_owned(),
        Value::String(omission.pointer.kind.clone()),
    );
    if let Some(path) = &omission.pointer.path {
        pointer.insert("path".to_owned(), Value::String(path.clone()));
    }
    if let Some(id) = &omission.pointer.id {
        pointer.insert("id".to_owned(), Value::String(id.clone()));
    }
    if let Some(field) = &omission.pointer.field {
        pointer.insert("field".to_owned(), Value::String(field.clone()));
    }
    pointer.insert("resolvable".to_owned(), Value::Bool(omission.resolvable));
    if let Some(tool) = &omission.tool {
        pointer.insert("tool".to_owned(), Value::String(tool.clone()));
    }

    let mut state = Map::new();
    state.insert("version".to_owned(), Value::from(omission.version));
    state.insert(
        "target".to_owned(),
        Value::String(
            match omission.target {
                OmissionTarget::Result => "result_body",
                OmissionTarget::Arguments => "arguments",
            }
            .to_owned(),
        ),
    );
    state.insert("reason".to_owned(), Value::String(omission.reason.clone()));
    if let Some(original_chars) = omission.original_chars {
        state.insert("original_chars".to_owned(), Value::from(original_chars));
    }
    if let Some(original_bytes) = omission.original_bytes {
        state.insert("original_bytes".to_owned(), Value::from(original_bytes));
    }
    if let Some(original_tokens) = omission.original_tokens {
        state.insert("original_tokens".to_owned(), Value::from(original_tokens));
    }
    state.insert("pointer".to_owned(), Value::Object(pointer));

    let mut root = Map::new();
    root.insert("opencrab_omission".to_owned(), Value::Object(state));
    Value::Object(root).to_string()
}

fn result_body_wire(body: &ResultBody) -> String {
    match body {
        ResultBody::Inline(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
        }
        ResultBody::Omitted(omission) => omission_wire_json(omission),
    }
}

fn ensure_tool_results_paired(history: &mut Vec<Message>, synthetic_count: &mut usize) {
    let result_ids: HashSet<String> = history
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.tool_call_id.clone())
        .collect();
    let mut paired = Vec::with_capacity(history.len());

    for message in history.drain(..) {
        let missing_ids: Vec<String> = if message.role == Role::Assistant {
            message
                .tool_calls
                .as_ref()
                .into_iter()
                .flatten()
                .filter(|call| !result_ids.contains(&call.id))
                .map(|call| call.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        paired.push(message);
        for call_id in missing_ids {
            paired.push(Message {
                role: Role::Tool,
                content: Some(MessageContent::Text(
                    r#"{"opencrab_omission":{"version":1,"target":"result_body","reason":"result_not_recorded","pointer":{"kind":"unavailable","resolvable":false}}}"#
                        .to_owned(),
                )),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: Some(call_id),
            });
            *synthetic_count += 1;
        }
    }

    *history = paired;
}

pub(crate) fn assemble_typed_messages(items: &[TypedItem]) -> AssembledTyped {
    let mut history = Vec::with_capacity(items.len());
    let mut machine_block_count = 0;

    let mut index = 0;
    while index < items.len() {
        // #884 PR2: 同一生成の並列 ToolCall（間に speech/result を挟まない連続 ToolCall item）を
        // 1 つの assistant message に複数 tool_calls として束ねる。1 呼び出し=別 assistant message に
        // すると anthropic が assistant(tool_use) の連続を 400 で拒否するため（対応 ToolResult は
        // 連続 Role::Tool になり anthropic 側の既存併合が効く）。
        if matches!(items[index], TypedItem::ToolCall { .. }) {
            let mut calls = Vec::new();
            while let Some(TypedItem::ToolCall {
                call_id,
                tool_name,
                arguments,
                ..
            }) = items.get(index)
            {
                calls.push(MessageToolCall {
                    id: call_id.clone(),
                    call_type: "function".to_owned(),
                    function: FunctionCall {
                        name: tool_name.clone(),
                        arguments: serde_json::to_string(arguments)
                            .unwrap_or_else(|_| arguments.to_string()),
                    },
                });
                index += 1;
            }
            history.push(Message {
                role: Role::Assistant,
                content: None,
                name: None,
                function_call: None,
                tool_calls: Some(calls),
                tool_call_id: None,
            });
            continue;
        }
        let message = match &items[index] {
            TypedItem::UserSpeech {
                event_ref,
                speaker,
                timestamp,
                content,
                relation,
            } => {
                // #884 PR2 §9.4-2: renderer 生成の固定ラベル 1 行 + 改行 + 本文。
                // #892: 話者はラベル 1 行に含めるのが正で、Message.name は残さない。
                // name を残すと ChatGPT Responses API が input item の `name` を 400 で拒否する。
                let label = user_speech_label(speaker, timestamp, event_ref, relation);
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text(format!("{label}\n{content}"))),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                }
            }
            TypedItem::AssistantSpeech { content, .. } => Message {
                role: Role::Assistant,
                content: Some(MessageContent::Text(content.clone())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            // ToolCall はループ先頭で束ねて処理済み。
            TypedItem::ToolCall { .. } => unreachable!("ToolCall はループ先頭で処理する"),
            TypedItem::ToolResult { call_id, body, .. } => Message {
                role: Role::Tool,
                content: Some(MessageContent::Text(result_body_wire(body))),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: Some(call_id.clone()),
            },
            TypedItem::MachineEvent { kind, payload, .. } => {
                machine_block_count += 1;
                let payload =
                    serde_json::to_string(payload).unwrap_or_else(|_| payload.to_string());
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text(format!(
                        "{MACHINE_HEADER}\n{kind}\n{payload}"
                    ))),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                }
            }
            TypedItem::ContextSection { content, .. } => {
                machine_block_count += 1;
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text(format!("{MACHINE_HEADER}\n{content}"))),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                }
            }
        };
        history.push(message);
        index += 1;
    }

    let mut synthetic_result_count = 0;
    ensure_tool_results_paired(&mut history, &mut synthetic_result_count);
    AssembledTyped {
        history,
        machine_block_count,
        synthetic_result_count,
    }
}

// Round 2 の engine 配線前も公開予定 API のシグネチャをコンパイル時に固定する。
const _: fn(&[TypedItem]) -> AssembledTyped = assemble_typed_messages;
const _: fn(&AssembledTyped) = |assembled| {
    let _ = (
        &assembled.history,
        assembled.machine_block_count,
        assembled.synthetic_result_count,
    );
};

pub fn build_typed_conversation(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
    conversation_high: usize,
    _conversation_low: usize,
    include_memory_index: bool,
    keep_response_directive: bool,
) -> Result<TypedConversation, anyhow::Error> {
    let snapshot = opencrab_db::queries::latest_conversation_snapshot(conn, session_id)?;
    let delta_logs = crate::conversation::retain_conversation_logs(match &snapshot {
        Some(snapshot) => opencrab_db::queries::list_session_logs_after(
            conn,
            session_id,
            snapshot.through_log_id,
        )?,
        None => opencrab_db::queries::list_session_logs_by_session(conn, session_id)?,
    });
    let all = crate::conversation::retain_conversation_logs(
        opencrab_db::queries::list_session_logs_by_session(conn, session_id)?,
    );
    let refs = crate::conversation::ConversationRefs::build(&all, agent_id);
    let completed: HashSet<String> = all
        .iter()
        .filter(|log| log.log_type == "tool_result" || log.log_type == "tool_cancelled")
        .filter_map(|log| result_metadata(log).0)
        .collect();
    let derived = derive_items(&delta_logs, &refs, &completed, agent_id);
    let assembled = assemble_typed_messages(&derived.items);

    let snapshot_base = snapshot.as_ref().and_then(|snapshot| {
        let base = crate::conversation::restore_frozen_snapshot(&snapshot.compacted_conversation);
        if base.is_empty() || base == crate::conversation::NO_MESSAGES_MARKER {
            None
        } else {
            Some(Message {
                role: Role::User,
                content: Some(MessageContent::Text(format!(
                    "{MACHINE_HEADER}\n[prior compacted conversation]\n{base}"
                ))),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            })
        }
    });

    let ledger = match crate::task_ledger::build_ledger_section(conn, agent_id, session_id) {
        Ok(section) => section,
        Err(error) => {
            tracing::warn!("failed to build task ledger section for session {session_id}: {error}");
            None
        }
    };
    let memory_index = if include_memory_index {
        match crate::memory_index::build_memory_index_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(error) => {
                tracing::warn!(
                    "failed to build memory index section for session {session_id}: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    let impressions =
        match crate::impression_section::build_impression_section(conn, agent_id, session_id) {
            Ok(section) => section,
            Err(error) => {
                tracing::warn!(
                    "failed to build impression section for session {session_id}: {error}"
                );
                None
            }
        };
    let context = [ledger, memory_index, impressions]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");
    let context_block = if context.is_empty() {
        None
    } else {
        Some(Message {
            role: Role::User,
            content: Some(MessageContent::Text(format!("{MACHINE_HEADER}\n{context}"))),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        })
    };

    let response_directive =
        if keep_response_directive && (!assembled.history.is_empty() || snapshot_base.is_some()) {
            Some(crate::conversation::RESPONSE_ONLY_DIRECTIVE.to_string())
        } else {
            None
        };

    let mut wire = String::new();
    for message in context_block
        .iter()
        .chain(snapshot_base.iter())
        .chain(assembled.history.iter())
    {
        wire.push_str(&serde_json::to_string(message)?);
    }
    if let Some(directive) = &response_directive {
        wire.push_str(directive);
    }
    let wire_tokens = crate::tokens::estimate_tokens(&wire);

    if wire_tokens > conversation_high {
        tracing::warn!(
            session_id,
            wire_tokens,
            conversation_high,
            "typed wire tokens exceed conversation_high (PR2: no typed compaction; relies on snapshot)"
        );
    } else {
        tracing::debug!(
            session_id,
            wire_tokens,
            items = derived.diagnostics.item_count,
            unpaired = derived.diagnostics.unpaired_call_count,
            opaque = derived.diagnostics.opaque_event_count,
            synthetic = assembled.synthetic_result_count,
            "typed conversation built"
        );
    }

    Ok(TypedConversation {
        context_block,
        snapshot_base,
        history: assembled.history,
        response_directive,
        wire_tokens,
        diagnostics: derived.diagnostics,
    })
}

