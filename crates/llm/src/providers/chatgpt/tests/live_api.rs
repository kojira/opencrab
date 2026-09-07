/// Model used by the real-API (`--ignored`) tests. ChatGPT/Codex accounts
/// reject `gpt-4o`, so we use the provider default path (`gpt-5.5`).
const TEST_MODEL: &str = DEFAULT_MODEL;

/// A system message for the real-API tests. Without it
/// `build_request_body` emits no `instructions` field and the API
/// rejects the request with HTTP 400 "Instructions are required".
fn real_test_system() -> Message {
    Message::system("You are a helpful assistant.")
}

#[tokio::test]
#[ignore]
async fn test_real_chatgpt_api() {
    // Uses real ~/.codex/auth.json — run with: cargo test -- --ignored
    let provider = ChatGptProvider::new();
    let request = ChatRequest {
        model: TEST_MODEL.to_string(),
        messages: vec![
            real_test_system(),
            Message {
                role: Role::User,
                content: Some(MessageContent::Text(
                    "Say exactly: hello from test".to_string(),
                )),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ],
        functions: None,
        function_call: None,
        temperature: None,
        max_tokens: Some(50),
        stop: None,
        stream: Some(false),
        metadata: std::collections::HashMap::new(),
        agent_id: None,
        reasoning_effort: None,
    };
    let response = provider.chat_completion(request).await;
    assert!(response.is_ok(), "API call failed: {:?}", response.err());
    let resp = response.unwrap();
    assert!(!resp.choices.is_empty(), "No choices returned");
    let content = match &resp.choices[0].message.content {
        Some(MessageContent::Text(t)) => t.clone(),
        _ => panic!("Expected text content"),
    };
    assert!(!content.is_empty(), "Empty response content");
    println!("Response: {}", content);
}

/// Build a simple weather function tool used by the real-API tool tests.
fn weather_tool() -> FunctionDefinition {
    FunctionDefinition {
        name: "get_current_weather".to_string(),
        description: Some("Get the current weather for a given city.".to_string()),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name, e.g. Tokyo"}
            },
            "required": ["city"],
            "additionalProperties": false
        }),
    }
}

/// Real API: the model can emit a parseable tool call.
/// Run with: cargo test -- --ignored
#[tokio::test]
#[ignore]
async fn test_real_chatgpt_api_tool_call() {
    let provider = ChatGptProvider::new();
    let request = ChatRequest {
        model: TEST_MODEL.to_string(),
        messages: vec![
            real_test_system(),
            Message::user(
                "What is the current weather in Tokyo? Call the get_current_weather tool to find out.",
            ),
        ],
        functions: Some(vec![weather_tool()]),
        // Force the call so the test is deterministic.
        function_call: Some(FunctionCallBehavior::Named {
            name: "get_current_weather".to_string(),
        }),
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: Some(false),
        metadata: std::collections::HashMap::new(),
        agent_id: None,
        reasoning_effort: None,
    };
    let response = provider.chat_completion(request).await;
    // On 400 the provider bails with the full HTTP body — surface it here.
    assert!(
        response.is_ok(),
        "tool-call API request failed (inspect for HTTP 400 detail): {:?}",
        response.err()
    );
    let resp = response.unwrap();
    assert!(!resp.choices.is_empty(), "no choices returned");
    assert_eq!(
        resp.choices[0].finish_reason,
        Some(FinishReason::ToolCalls),
        "expected the model to emit a tool call"
    );
    let calls = resp.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool_calls must be present");
    assert!(!calls.is_empty(), "tool_calls vec must not be empty");
    let call = &calls[0];
    assert_eq!(
        call.function.name, "get_current_weather",
        "unexpected tool name"
    );
    let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
        .unwrap_or_else(|e| {
            panic!(
                "tool arguments must be valid JSON ({e}): {}",
                call.function.arguments
            )
        });
    assert!(
        args.get("city").is_some(),
        "expected a 'city' argument, got: {}",
        call.function.arguments
    );
    println!(
        "Tool call: {} args={}",
        call.function.name, call.function.arguments
    );
}

/// Real API: a continuation request after tool execution must NOT 400.
/// First force a tool call, then send back the tool result as a
/// function_call_output and assert the model produces a final answer.
/// Run with: cargo test -- --ignored
#[tokio::test]
#[ignore]
async fn test_real_chatgpt_api_tool_continuation() {
    let provider = ChatGptProvider::new();
    let user = Message::user(
        "What is the current weather in Tokyo? Call the get_current_weather tool.",
    );

    // Phase 1: force the tool call.
    let first = ChatRequest {
        model: TEST_MODEL.to_string(),
        messages: vec![real_test_system(), user.clone()],
        functions: Some(vec![weather_tool()]),
        function_call: Some(FunctionCallBehavior::Named {
            name: "get_current_weather".to_string(),
        }),
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: Some(false),
        metadata: std::collections::HashMap::new(),
        agent_id: None,
        reasoning_effort: None,
    };
    let first_resp = provider
        .chat_completion(first)
        .await
        .unwrap_or_else(|e| panic!("first (tool-call) request failed: {e:?}"));
    let calls = first_resp.choices[0]
        .message
        .tool_calls
        .clone()
        .expect("expected a tool call in the first response");
    assert!(!calls.is_empty(), "tool_calls must not be empty");

    // Phase 2: assistant message carrying the tool calls + tool results.
    let mut assistant = Message::assistant("");
    assistant.tool_calls = Some(calls.clone());

    let mut messages = vec![real_test_system(), user, assistant];
    for c in &calls {
        messages.push(Message::tool(
            c.id.clone(),
            r#"{"temperature_c":22,"condition":"Sunny"}"#,
        ));
    }

    let second = ChatRequest {
        model: TEST_MODEL.to_string(),
        messages,
        functions: Some(vec![weather_tool()]),
        // Let the model produce the final text answer.
        function_call: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        stream: Some(false),
        metadata: std::collections::HashMap::new(),
        agent_id: None,
        reasoning_effort: None,
    };
    let response = provider.chat_completion(second).await;
    assert!(
        response.is_ok(),
        "continuation request failed — must NOT be HTTP 400 (full error): {:?}",
        response.err()
    );
    let final_resp = response.unwrap();
    let text = final_resp.first_text().unwrap_or("");
    assert!(
        !text.is_empty(),
        "final continuation text must not be empty; finish_reason={:?}",
        final_resp
            .choices
            .first()
            .and_then(|c| c.finish_reason.clone())
    );
    println!("Continuation final text: {}", text);
}
