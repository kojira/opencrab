use super::*;

fn base_request() -> ChatRequest {
    ChatRequest {
        model: "claude-x".to_string(),
        messages: vec![Message::system("sys prompt"), Message::user("hi")],
        temperature: None,
        max_tokens: Some(100),
        stop: None,
        stream: None,
        agent_id: None,
        reasoning_effort: None,
        metadata: Default::default(),
        functions: Some(vec![
            FunctionDefinition {
                name: "a".to_string(),
                description: Some("d".to_string()),
                parameters: serde_json::json!({"type": "object"}),
            },
            FunctionDefinition {
                name: "b".to_string(),
                description: Some("d".to_string()),
                parameters: serde_json::json!({"type": "object"}),
            },
        ]),
        function_call: None,
    }
}

/// プロンプトキャッシュはプロバイダの能力（#44）: system は cache_control 付き
/// text ブロック配列、tools は最後の定義にのみ cache_control が付くこと。
#[test]
fn cache_policy_applied_by_provider() {
    let provider = AnthropicProvider::new("k");
    let body = provider
        .build_request_body(&base_request())
        .expect("valid request builds a body");

    let system = body["system"].as_array().expect("system must be blocks");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "sys prompt");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(system[0]["cache_control"]["ttl"], "1h");

    let tools = body["tools"].as_array().unwrap();
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

    // 最後のメッセージの最終ブロックに incremental cache マーカーが付く
    // （文字列 content はブロック配列へ変換される）。
    let messages = body["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    let blocks = last["content"].as_array().expect("last content is blocks");
    let last_block = blocks.last().unwrap();
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");
    // 5m 既定 TTL（ttl キー無し）— 1h を明示するのは system/tools のみ
    assert!(last_block["cache_control"].get("ttl").is_none());
    // 先行メッセージにはマーカーが無い
    for msg in &messages[..messages.len() - 1] {
        match &msg["content"] {
            serde_json::Value::Array(blocks) => {
                for b in blocks {
                    assert!(b.get("cache_control").is_none());
                }
            }
            v => assert!(v.is_string()),
        }
    }
}

/// tools 無しの単発呼び出し（evaluator / ロールアップ等）にはメッセージ側の
/// キャッシュマーカーを付けない（書き込み割増 +25% に対して後続ヒットが無い）。
#[test]
fn no_message_cache_marker_without_tools() {
    let provider = AnthropicProvider::new("k");
    let mut req = base_request();
    req.functions = None;
    let body = provider
        .build_request_body(&req)
        .expect("valid request builds a body");
    let messages = body["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    assert!(
        last["content"].is_string(),
        "content must stay a plain string without tools"
    );
}

/// 複数 system メッセージは連結して1ブロックになる（旧挙動の保存）。
#[test]
fn multiple_system_messages_concatenated() {
    let mut req = base_request();
    req.messages.insert(1, Message::system("second sys"));
    let provider = AnthropicProvider::new("k");
    let body = provider
        .build_request_body(&req)
        .expect("valid request builds a body");
    let system = body["system"].as_array().unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "sys prompt\n\nsecond sys");
}

/// max_tokens が None のときは任意定数へ黙って落とさず fail loud する（#681）。
/// Anthropic の messages API は max_tokens を必須で要求するため省略もできない。
#[test]
fn missing_max_tokens_fails_loud() {
    let mut req = base_request();
    req.max_tokens = None;
    let provider = AnthropicProvider::new("k");
    let err = provider
        .build_request_body(&req)
        .expect_err("None max_tokens must be a loud error, not a silent default");
    assert!(
        err.to_string().contains("max_tokens"),
        "error should name the missing field: {err}"
    );
}

#[test]
fn test_typed_history_past_turn_converts_to_tool_use_result_blocks() {
    let provider = AnthropicProvider::new("k");
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
        "claude-x",
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
    )
    .with_max_tokens(100);

    let body = provider
        .build_request_body(&request)
        .expect("valid typed history builds a body");
    let messages = body["messages"]
        .as_array()
        .expect("messages must be an array");

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], serde_json::json!("user"));
    assert_eq!(
        messages[0]["content"],
        serde_json::json!("くらぶ、60秒sleepして終わったら教えて")
    );

    assert_eq!(messages[1]["role"], serde_json::json!("assistant"));
    let tool_use_blocks = messages[1]["content"]
        .as_array()
        .expect("assistant content must be blocks");
    assert_eq!(tool_use_blocks.len(), 1);
    assert_eq!(tool_use_blocks[0]["type"], serde_json::json!("tool_use"));
    assert_eq!(tool_use_blocks[0]["id"], serde_json::json!("call_c253"));
    assert_eq!(
        tool_use_blocks[0]["name"],
        serde_json::json!("execute_shell")
    );
    let tool_input = tool_use_blocks[0]["input"]
        .as_object()
        .expect("tool_use input must be an object");
    assert_eq!(tool_input["command"], serde_json::json!("sleep"));

    assert_eq!(messages[2]["role"], serde_json::json!("user"));
    let tool_result_blocks = messages[2]["content"]
        .as_array()
        .expect("tool result content must be blocks");
    assert_eq!(tool_result_blocks.len(), 1);
    assert_eq!(
        tool_result_blocks[0]["type"],
        serde_json::json!("tool_result")
    );
    assert_eq!(
        tool_result_blocks[0]["tool_use_id"],
        serde_json::json!("call_c253")
    );

    assert_eq!(messages[3]["role"], serde_json::json!("user"));
    assert_eq!(messages[3]["content"], serde_json::json!("終わった？"));

    fn contains_log_marker(value: &Value) -> bool {
        match value {
            Value::String(text) => text.contains("→log:"),
            Value::Array(values) => values.iter().any(contains_log_marker),
            Value::Object(map) => map.values().any(contains_log_marker),
            _ => false,
        }
    }
    assert!(!messages.iter().any(contains_log_marker));
}

#[test]
fn test_message_name_not_emitted_on_wire() {
    // #892 回帰固定: anthropic wire は message の `name` を出さない
    // （anthropic API に name フィールドは無い）。話者は本文へ埋め込む。
    let provider = AnthropicProvider::new("k");
    let mut user = Message::user("終わった？");
    user.name = Some("owner".to_string());
    let request =
        ChatRequest::new("claude-x", vec![Message::system("sys"), user]).with_max_tokens(100);

    let body = provider
        .build_request_body(&request)
        .expect("valid request builds a body");
    let messages = body["messages"]
        .as_array()
        .expect("messages must be an array");

    assert_eq!(messages.len(), 1, "system は messages から除かれる");
    assert_eq!(messages[0]["role"], serde_json::json!("user"));
    assert!(
        messages[0].get("name").is_none(),
        "anthropic wire message に name キーを出さない: {}",
        messages[0]
    );
}

/// #884 PR2 並列呼び出し: 同一生成の並列 ToolCall（1 assistant に tool_calls×2）は
/// anthropic wire で `tool_use`×2 を 1 つの assistant message へ入れ、対応する連続
/// Role::Tool は `tool_result`×2 を 1 つの user message へ併合する（tool_use の
/// 連続や tool_result の分割で 400 を招かない）。core の集約
/// (`assemble_parallel_calls_grouped_into_one_assistant`) と対になる provider snapshot。
#[test]
fn parallel_tool_calls_become_two_tool_use_in_one_assistant_and_merged_tool_results() {
    let provider = AnthropicProvider::new("k");
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
        "claude-x",
        vec![
            Message::system("sys"),
            Message::user("並列で a と b を実行して"),
            assistant,
            Message::tool("call_a", r#"{"exit_code":0,"stdout":"a"}"#),
            Message::tool("call_b", r#"{"exit_code":0,"stdout":"b"}"#),
        ],
    )
    .with_max_tokens(100);

    let body = provider
        .build_request_body(&request)
        .expect("valid parallel typed history builds a body");
    let messages = body["messages"]
        .as_array()
        .expect("messages must be an array");

    // user(発話) + assistant(tool_use×2) + user(tool_result×2 併合) の 3 本。
    assert_eq!(messages.len(), 3, "並列 result は 1 user に併合され計 3 本");

    // 1 本目: 発話。
    assert_eq!(messages[0]["role"], serde_json::json!("user"));

    // 2 本目: 1 つの assistant に tool_use が 2 個。
    assert_eq!(messages[1]["role"], serde_json::json!("assistant"));
    let tool_use_blocks = messages[1]["content"]
        .as_array()
        .expect("assistant content must be blocks");
    assert_eq!(
        tool_use_blocks.len(),
        2,
        "並列 2 呼び出しが 1 assistant に集約"
    );
    assert_eq!(tool_use_blocks[0]["type"], serde_json::json!("tool_use"));
    assert_eq!(tool_use_blocks[0]["id"], serde_json::json!("call_a"));
    assert_eq!(tool_use_blocks[1]["type"], serde_json::json!("tool_use"));
    assert_eq!(tool_use_blocks[1]["id"], serde_json::json!("call_b"));

    // 3 本目: 連続 Role::Tool が 1 user の tool_result×2 に併合。
    assert_eq!(messages[2]["role"], serde_json::json!("user"));
    let tool_result_blocks = messages[2]["content"]
        .as_array()
        .expect("tool result content must be blocks");
    assert_eq!(
        tool_result_blocks.len(),
        2,
        "連続 tool_result が 1 user に併合"
    );
    assert_eq!(
        tool_result_blocks[0]["type"],
        serde_json::json!("tool_result")
    );
    assert_eq!(
        tool_result_blocks[0]["tool_use_id"],
        serde_json::json!("call_a")
    );
    assert_eq!(
        tool_result_blocks[1]["tool_use_id"],
        serde_json::json!("call_b")
    );

    // assistant(tool_use) が連続しない（anthropic 400 の条件を作らない）。
    for pair in messages.windows(2) {
        assert!(
            !(pair[0]["role"] == "assistant" && pair[1]["role"] == "assistant"),
            "assistant が連続してはならない"
        );
    }
}

/// リクエストを受けてから `delay` 待って 200 を返すモック（timeout 検証用）。
async fn spawn_slow_mock(delay: Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let resp = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}/slow")
}

/// #667: 総時間 timeout が実際に client へ効いていることを確認する。無応答の上流を
/// 有限で切る（fail loud）ための肝なので、定数の保持ではなく client の挙動で見る。
#[tokio::test]
async fn test_chat_timeout_is_applied_to_the_http_client() {
    let url = spawn_slow_mock(Duration::from_millis(1500)).await;

    let short = build_client(1);
    let err = short.get(&url).send().await.unwrap_err();
    assert!(err.is_timeout(), "1 秒なら timeout するはず: {err}");

    let long = build_client(10);
    let resp = long.get(&url).send().await.expect("10 秒なら読み切れる");
    assert!(resp.status().is_success());
}
