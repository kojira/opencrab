#[test]
fn test_build_request_body_max_output_tokens() {
    // max_output_tokens must NOT appear in the request body (unsupported by the API).
    let provider = ChatGptProvider::new();
    let mut request = ChatRequest::new("gpt-5.5", vec![Message::user("hi")]);
    request.max_tokens = Some(256);
    let body = provider.build_request_body(&request, false);
    assert!(
        body.get("max_output_tokens").is_none(),
        "max_output_tokens must not be sent to the API"
    );

    let request_none = ChatRequest::new("gpt-5.5", vec![Message::user("hi")]);
    let body_none = provider.build_request_body(&request_none, false);
    assert!(body_none.get("max_output_tokens").is_none());
}

/// #884 PR2 並列呼び出し: 同一生成の並列 ToolCall（1 assistant に tool_calls×2）は
/// Responses(chatgpt) wire で `function_call`×2 の input アイテムに展開し、対応する
/// 連続 Role::Tool は `function_call_output`×2 に展開する（call_id で 1:1 対応）。
/// core の集約 (`assemble_parallel_calls_grouped_into_one_assistant`) と対になる
/// provider snapshot。
#[test]
fn parallel_tool_calls_expand_to_two_function_calls_and_two_outputs() {
    let provider = ChatGptProvider::new();
    let mut assistant = Message::assistant("");
    assistant.content = None;
    assistant.tool_calls = Some(vec![
        ToolCall {
            id: "call_a".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "execute_shell".to_string(),
                arguments: r#"{"command":"echo","args":["a"]}"#.to_string(),
            },
        },
        ToolCall {
            id: "call_b".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "execute_shell".to_string(),
                arguments: r#"{"command":"echo","args":["b"]}"#.to_string(),
            },
        },
    ]);
    let request = ChatRequest::new(
        "gpt-5.5",
        vec![
            Message::system("sys"),
            Message::user("並列で a と b を実行して"),
            assistant,
            Message::tool("call_a", r#"{"exit_code":0,"stdout":"a"}"#),
            Message::tool("call_b", r#"{"exit_code":0,"stdout":"b"}"#),
        ],
    );

    let body = provider.build_request_body(&request, false);
    let input = body["input"].as_array().expect("input must be an array");

    // function_call が 2 個・function_call_output が 2 個そろう。
    let function_calls: Vec<&Value> = input
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    let function_outputs: Vec<&Value> = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    assert_eq!(
        function_calls.len(),
        2,
        "並列 2 呼び出しが function_call×2 に展開"
    );
    assert_eq!(
        function_outputs.len(),
        2,
        "並列 2 結果が function_call_output×2 に展開"
    );

    // call_id が 1:1 対応で保存される。
    assert_eq!(function_calls[0]["call_id"], serde_json::json!("call_a"));
    assert_eq!(function_calls[1]["call_id"], serde_json::json!("call_b"));
    assert_eq!(function_outputs[0]["call_id"], serde_json::json!("call_a"));
    assert_eq!(function_outputs[1]["call_id"], serde_json::json!("call_b"));

    // 順序: 2 つの function_call が両 output より前に並ぶ（生成→結果の順）。
    let first_output_pos = input
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .expect("function_call_output must exist");
    let last_call_pos = input
        .iter()
        .rposition(|item| item["type"] == "function_call")
        .expect("function_call must exist");
    assert!(
        last_call_pos < first_output_pos,
        "function_call は function_call_output より前"
    );
}

#[test]
fn test_reasoning_effort_never_emits_max_output_tokens() {
    // max_output_tokens は Responses API 非対応のため、いかなる設定でも body に現れない。
    let low = ChatGptProvider::new().with_reasoning_effort("low");
    let body_low = low.build_request_body(
        &ChatRequest::new("gpt-5.5", vec![Message::user("hi")]),
        false,
    );
    assert!(
        body_low.get("max_output_tokens").is_none(),
        "max_output_tokens must not be sent to the API"
    );

    let high = ChatGptProvider::new().with_reasoning_effort("high");
    let body_high = high.build_request_body(
        &ChatRequest::new("gpt-5.5", vec![Message::user("hi")]),
        false,
    );
    assert!(body_high.get("max_output_tokens").is_none());
}

/// metadata の web_search=true で native web_search ツールが tools に載ること
/// （codex CLI が同じバックエンドへ送るのと同じ形）。未設定なら載らない。
#[test]
fn test_build_request_body_web_search_tool() {
    let provider = ChatGptProvider::new();

    // 有効時: function ツールと併存して web_search が入る。
    let mut request = ChatRequest::new("gpt-5.6-sol", vec![Message::user("このURLを見て")]);
    request
        .metadata
        .insert("web_search".to_string(), serde_json::json!(true));
    request.functions = Some(vec![FunctionDefinition {
        name: "my_tool".to_string(),
        description: None,
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }]);
    let body = provider.build_request_body(&request, false);
    let tools = body["tools"].as_array().expect("tools array");
    assert!(tools.iter().any(|t| t["type"] == "function"));
    let ws = tools
        .iter()
        .find(|t| t["type"] == "web_search")
        .expect("web_search tool present");
    assert_eq!(ws["external_web_access"], true);
    assert_eq!(
        ws["search_content_types"],
        serde_json::json!(["text", "image"])
    );

    // 未設定時: web_search は載らない（functions 無しなら tools 自体無し）。
    let plain = ChatRequest::new("gpt-5.6-sol", vec![Message::user("hi")]);
    let body = provider.build_request_body(&plain, false);
    assert!(body.get("tools").is_none());
}

#[test]
fn test_build_request_body_converts_assistant_tool_calls_to_function_call_items() {
    let provider = ChatGptProvider::new();
    let mut assistant = Message::assistant("");
    assistant.tool_calls = Some(vec![ToolCall {
        id: "call_1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "get_weather".to_string(),
            arguments: r#"{"city":"Tokyo"}"#.to_string(),
        },
    }]);
    let request =
        ChatRequest::new("gpt-5.5", vec![Message::user("hi"), assistant]).with_max_tokens(256);

    let body = provider.build_request_body(&request, false);

    assert!(body.get("max_output_tokens").is_none());
    let input = body["input"].as_array().expect("input must be an array");
    assert_eq!(input.len(), 2);
    assert_eq!(input[1]["type"], serde_json::json!("function_call"));
    assert_eq!(input[1]["call_id"], serde_json::json!("call_1"));
    assert_eq!(input[1]["name"], serde_json::json!("get_weather"));
    assert_eq!(
        input[1]["arguments"],
        serde_json::json!(r#"{"city":"Tokyo"}"#)
    );
    assert!(
        input[1].get("role").is_none(),
        "function_call input items must not be role messages"
    );
    assert!(
        input[1].get("content").is_none(),
        "function_call input items must not require content"
    );

    fn contains_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(key) || map.values().any(|v| contains_key(v, key))
            }
            Value::Array(values) => values.iter().any(|v| contains_key(v, key)),
            _ => false,
        }
    }
    assert!(
        !contains_key(&body, "tool_calls"),
        "tool_calls must not be sent to the Responses API"
    );
    assert!(
        !contains_key(&body, "tool_call_id"),
        "tool_call_id must not be sent to the Responses API"
    );
}

#[test]
fn test_build_request_body_keeps_assistant_text_alongside_tool_calls() {
    let provider = ChatGptProvider::new();
    let mut assistant = Message::assistant("I'll check the weather");
    assistant.tool_calls = Some(vec![ToolCall {
        id: "call_1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "get_weather".to_string(),
            arguments: r#"{"city":"Tokyo"}"#.to_string(),
        },
    }]);
    let request = ChatRequest::new("gpt-5.5", vec![Message::user("hi"), assistant]);

    let body = provider.build_request_body(&request, false);
    let input = body["input"].as_array().expect("input must be an array");

    // user, assistant-text, function_call の3要素になる。
    assert_eq!(input.len(), 3);
    assert_eq!(input[1]["role"], serde_json::json!("assistant"));
    assert!(input[1]["content"]
        .to_string()
        .contains("I'll check the weather"));
    assert_eq!(input[2]["type"], serde_json::json!("function_call"));
}

#[test]
fn test_build_request_body_converts_tool_result_to_function_call_output_item() {
    let provider = ChatGptProvider::new();
    let mut assistant = Message::assistant("");
    assistant.tool_calls = Some(vec![ToolCall {
        id: "call_1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "get_weather".to_string(),
            arguments: r#"{"city":"Tokyo"}"#.to_string(),
        },
    }]);
    let tool = Message::tool("call_1", r#"{"temperature":22}"#);
    let request = ChatRequest::new("gpt-5.5", vec![Message::user("hi"), assistant, tool]);

    let body = provider.build_request_body(&request, false);

    let input = body["input"].as_array().expect("input must be an array");
    assert_eq!(input.len(), 3);
    assert_eq!(input[1]["type"], serde_json::json!("function_call"));
    assert_eq!(input[2]["type"], serde_json::json!("function_call_output"));
    assert_eq!(input[2]["call_id"], serde_json::json!("call_1"));
    assert_eq!(
        input[2]["output"],
        serde_json::json!(r#"{"temperature":22}"#)
    );
    assert!(
        input[2].get("role").is_none(),
        "function_call_output input items must not be role messages"
    );
    assert!(
        input[2].get("content").is_none(),
        "function_call_output input items must not use message content"
    );
    assert!(body.get("max_output_tokens").is_none());
}

#[test]
fn test_typed_history_past_turn_converts_to_function_call_items() {
    let provider = ChatGptProvider::new();
    let mut assistant = Message::assistant("");
    assistant.content = None;
    assistant.tool_calls = Some(vec![ToolCall {
        id: "call_c253".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "execute_shell".to_string(),
            arguments: r#"{"args":["60"],"command":"sleep","stdin":"","timeout_secs":90}"#
                .to_string(),
        },
    }]);
    let request = ChatRequest::new(
        "gpt-5.5",
        vec![
            Message::system("sys"),
            Message::user("くらぶ、60秒sleepして終わったら教えて"),
            assistant,
            Message::tool(
                "call_c253",
                r#"{"status":"completed","exit_code":0,"stdout":""}"#,
            ),
            Message::user("終わった？"),
        ],
    );

    let body = provider.build_request_body(&request, false);
    let input = body["input"].as_array().expect("input must be an array");

    assert_eq!(input.len(), 4);
    assert_eq!(input[0]["role"], serde_json::json!("user"));
    assert!(input[0]["content"]
        .as_str()
        .is_some_and(|text| text.contains("くらぶ、60秒sleepして終わったら教えて")));

    assert_eq!(input[1]["type"], serde_json::json!("function_call"));
    assert_eq!(input[1]["call_id"], serde_json::json!("call_c253"));
    assert_eq!(input[1]["name"], serde_json::json!("execute_shell"));
    let arguments = input[1]["arguments"]
        .as_str()
        .expect("function_call arguments must be a string");
    assert!(arguments.contains("sleep"));
    assert!(arguments.contains("60"));

    assert_eq!(input[2]["type"], serde_json::json!("function_call_output"));
    assert_eq!(input[2]["call_id"], serde_json::json!("call_c253"));
    assert!(input[2]["output"]
        .as_str()
        .is_some_and(|output| output.contains("exit_code")));
    assert!(
        input[2].get("role").is_none(),
        "function_call_output input items must not be role messages"
    );

    assert_eq!(input[3]["role"], serde_json::json!("user"));
    assert_eq!(input[3]["content"], serde_json::json!("終わった？"));

    fn contains_log_marker(value: &Value) -> bool {
        match value {
            Value::String(text) => text.contains("→log:"),
            Value::Array(values) => values.iter().any(contains_log_marker),
            Value::Object(map) => map.values().any(contains_log_marker),
            _ => false,
        }
    }
    assert!(!input.iter().any(contains_log_marker));
}

#[test]
fn test_responses_input_drops_message_name() {
    // #892: Responses API は input item の `name` を拒否する
    // （400 Unknown parameter: 'input[N].name'）。Message.name が設定されていても
    // wire の input item には name キーを出さない（防御）。
    let provider = ChatGptProvider::new();
    let mut user = Message::user("終わった？");
    user.name = Some("owner".to_string());
    let request = ChatRequest::new("gpt-5.5", vec![Message::system("sys"), user]);

    let body = provider.build_request_body(&request, false);
    let input = body["input"].as_array().expect("input must be an array");

    assert_eq!(input.len(), 1, "system は input から除かれる");
    assert_eq!(input[0]["role"], serde_json::json!("user"));
    assert!(
        input[0].get("name").is_none(),
        "input item に name キーを出さない: {}",
        input[0]
    );
}

// ── build_request_body field validation ──────────────────────────────────

#[test]
fn test_request_body_required_fields() {
    let provider = ChatGptProvider::new();
    let request = ChatRequest::new(
        "gpt-5.5",
        vec![Message::system("You are helpful."), Message::user("Hello")],
    );
    let body = provider.build_request_body(&request, false);

    // Required fields must be present.
    assert_eq!(body["model"], serde_json::json!("gpt-5.5"));
    assert!(body.get("input").is_some(), "input field must be present");
    assert_eq!(body["stream"], serde_json::json!(false));
    assert_eq!(body["store"], serde_json::json!(false));

    // max_output_tokens must NEVER appear (unsupported by Responses API).
    assert!(
        body.get("max_output_tokens").is_none(),
        "max_output_tokens must not be sent to the API"
    );
}

#[test]
fn test_request_body_stream_flag() {
    let provider = ChatGptProvider::new();
    let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
    let body_stream = provider.build_request_body(&request, true);
    assert_eq!(body_stream["stream"], serde_json::json!(true));
    let body_no_stream = provider.build_request_body(&request, false);
    assert_eq!(body_no_stream["stream"], serde_json::json!(false));
}

#[test]
fn test_request_body_reasoning_effort_low() {
    let provider = ChatGptProvider::new().with_reasoning_effort("low");
    let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
    let body = provider.build_request_body(&request, false);
    assert_eq!(
        body["reasoning"]["effort"],
        serde_json::json!("low"),
        "reasoning.effort must be 'low'"
    );
    assert!(body.get("max_output_tokens").is_none());
}

#[test]
fn test_request_body_reasoning_effort_high() {
    let provider = ChatGptProvider::new().with_reasoning_effort("high");
    let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
    let body = provider.build_request_body(&request, false);
    assert_eq!(
        body["reasoning"]["effort"],
        serde_json::json!("high"),
        "reasoning.effort must be 'high'"
    );
    assert!(body.get("max_output_tokens").is_none());
}

#[test]
fn test_request_body_no_reasoning_by_default() {
    // Default new() sets reasoning_effort = Some("low"), so "reasoning" WILL appear.
    // But if we explicitly clear it, it must not appear.
    let mut provider = ChatGptProvider::new();
    provider.reasoning_effort = None;
    let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
    let body = provider.build_request_body(&request, false);
    assert!(
        body.get("reasoning").is_none(),
        "reasoning field must not appear when reasoning_effort is None"
    );
}

#[test]
fn test_request_body_instructions_from_system_message() {
    let provider = ChatGptProvider::new();
    let request = ChatRequest::new(
        "gpt-5.5",
        vec![Message::system("Be concise."), Message::user("Hi")],
    );
    let body = provider.build_request_body(&request, false);
    let instructions = body["instructions"]
        .as_str()
        .expect("instructions must be set");
    assert!(
        instructions.contains("Be concise."),
        "instructions must include system message"
    );
}
