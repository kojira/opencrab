use super::*;

// ==================== 共通 helpers（qc_harness_e2e に準拠） ====================

pub(crate) fn request_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn has_tool_role(request: &ChatRequest) -> bool {
    request.messages.iter().any(|m| m.role == Role::Tool)
}

pub(crate) fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: Message::assistant(text),
            finish_reason: Some(FinishReason::Stop),
        }],
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        created: 0,
    }
}

pub(crate) fn tool_call_response(name: &str, args: serde_json::Value) -> ChatResponse {
    let msg = Message {
        role: Role::Assistant,
        content: None,
        name: None,
        function_call: None,
        tool_calls: Some(vec![ToolCall {
            id: format!("tc-{}", uuid::Uuid::new_v4()),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }]),
        tool_call_id: None,
    };
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: Some(FinishReason::ToolCalls),
        }],
        usage: Usage::default(),
        created: 0,
    }
}

/// 複数 tool_call を 1 生成に並べる（reply×N in one 用）。
pub(crate) fn tool_calls_response(calls: Vec<(&str, serde_json::Value)>) -> ChatResponse {
    let msg = Message {
        role: Role::Assistant,
        content: None,
        name: None,
        function_call: None,
        tool_calls: Some(
            calls
                .into_iter()
                .map(|(name, args)| ToolCall {
                    id: format!("tc-{}", uuid::Uuid::new_v4()),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: name.to_string(),
                        arguments: args.to_string(),
                    },
                })
                .collect(),
        ),
        tool_call_id: None,
    };
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: Some(FinishReason::ToolCalls),
        }],
        usage: Usage::default(),
        created: 0,
    }
}

/// reply tool_call と content を同一生成に載せる（reply＋本文/CONTINUE/NO_REPLY 併記用）。
pub(crate) fn reply_with_content_response(text: &str, content: &str) -> ChatResponse {
    let mut resp = tool_call_response("reply", serde_json::json!({"event": "e1", "text": text}));
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}

/// execute_shell tool_call と content（holding 宣言本文）を同一生成に載せる（#916 holding 用）。
/// §13 表 #10「query ツール（execute_shell 等）＋本文（holding）」の 1 生成を作る。
pub(crate) fn shell_with_content_response(
    content: &str,
    command: &str,
    args: &[&str],
) -> ChatResponse {
    let mut resp = tool_call_response(
        "execute_shell",
        serde_json::json!({ "command": command, "args": args }),
    );
    resp.choices[0].message.content = Some(MessageContent::Text(content.to_string()));
    resp
}
