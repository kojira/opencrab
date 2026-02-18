//! Integration tests for the OpenRouter provider.
//!
//! These tests call the real OpenRouter API and require `OPENROUTER_API_KEY` to be set.
//! They are marked `#[ignore]` so they won't run with normal `cargo test`.
//!
//! Run with:
//!   OPENROUTER_API_KEY="sk-or-..." cargo test -p opencrab-llm --test openrouter_integration -- --ignored

use opencrab_llm::message::*;
use opencrab_llm::providers::openrouter::OpenRouterProvider;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;
use std::sync::Arc;

fn api_key() -> String {
    std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set")
}

fn provider() -> OpenRouterProvider {
    OpenRouterProvider::new(api_key()).with_title("OpenCrab Test")
}

// ---------- Provider direct tests ----------

#[tokio::test]
#[ignore]
async fn test_health_check() {
    let p = provider();
    let healthy = p.health_check().await.unwrap();
    assert!(healthy, "OpenRouter API should be reachable");
}

#[tokio::test]
#[ignore]
async fn test_available_models() {
    let p = provider();
    let models = p.available_models().await.unwrap();
    assert!(!models.is_empty(), "OpenRouter should list models");

    // Spot-check a well-known model
    let has_gpt4 = models.iter().any(|m| m.id.contains("gpt-4"));
    assert!(has_gpt4, "Model list should include a GPT-4 variant");
}

#[tokio::test]
#[ignore]
async fn test_chat_completion_simple() {
    let p = provider();
    let request = ChatRequest::new(
        "openai/gpt-4o-mini",
        vec![
            Message::system("You are a helpful assistant. Reply in one short sentence."),
            Message::user("What is 2 + 3?"),
        ],
    )
    .with_max_tokens(50);

    let response = p.chat_completion(request).await.unwrap();

    assert!(!response.choices.is_empty(), "Should have at least one choice");
    let text = response.first_text().expect("Response should contain text");
    assert!(!text.is_empty(), "Response text should not be empty");
    assert!(
        text.contains('5'),
        "Response should contain '5', got: {text}"
    );
    assert!(response.usage.total_tokens > 0, "Usage tokens should be reported");
}

#[tokio::test]
#[ignore]
async fn test_chat_completion_with_temperature() {
    let p = provider();
    let request = ChatRequest::new(
        "openai/gpt-4o-mini",
        vec![Message::user("Say exactly: hello world")],
    )
    .with_temperature(0.0)
    .with_max_tokens(20);

    let response = p.chat_completion(request).await.unwrap();
    let text = response.first_text().unwrap().to_lowercase();
    assert!(
        text.contains("hello world"),
        "With temperature 0, should follow instruction closely, got: {text}"
    );
}

#[tokio::test]
#[ignore]
async fn test_chat_completion_tool_calling() {
    let p = provider();
    let weather_fn = FunctionDefinition {
        name: "get_weather".to_string(),
        description: Some("Get the current weather for a location".to_string()),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City name"
                }
            },
            "required": ["location"]
        }),
    };

    let request = ChatRequest {
        model: "openai/gpt-4o-mini".to_string(),
        messages: vec![Message::user("What's the weather in Tokyo?")],
        functions: Some(vec![weather_fn]),
        function_call: Some(FunctionCallBehavior::Mode("auto".to_string())),
        temperature: Some(0.0),
        max_tokens: Some(200),
        stop: None,
        stream: Some(false),
        metadata: Default::default(),
    };

    let response = p.chat_completion(request).await.unwrap();
    let choice = &response.choices[0];

    // The model should call get_weather
    let has_tool_call = choice
        .message
        .tool_calls
        .as_ref()
        .map(|tc| tc.iter().any(|t| t.function.name == "get_weather"))
        .unwrap_or(false);

    assert!(
        has_tool_call,
        "Model should call get_weather tool, got: {:?}",
        choice.message
    );
}

#[tokio::test]
#[ignore]
async fn test_chat_completion_streaming() {
    use futures::StreamExt;

    let p = provider();
    let request = ChatRequest::new(
        "openai/gpt-4o-mini",
        vec![Message::user("Count from 1 to 5, one number per line.")],
    )
    .with_max_tokens(50);

    let mut stream = p.chat_completion_stream(request).await.unwrap();

    let mut full_text = String::new();
    let mut chunk_count = 0;

    while let Some(result) = stream.next().await {
        match result {
            Ok(delta) => {
                for choice in &delta.choices {
                    if let Some(ref content) = choice.delta.content {
                        full_text.push_str(content);
                    }
                }
                chunk_count += 1;
            }
            Err(_) => {
                // Empty SSE chunks (e.g. keep-alive) can fail parsing; skip them
                continue;
            }
        }
    }

    assert!(chunk_count > 0, "Should receive at least one chunk");
    assert!(!full_text.is_empty(), "Streamed text should not be empty");
    // Verify at least some numbers came through (chunk boundaries may split data)
    let has_numbers = full_text.chars().any(|c| c.is_ascii_digit());
    assert!(
        has_numbers,
        "Streamed response should contain numbers, got: {full_text}"
    );
}

// ---------- Router integration tests ----------

#[tokio::test]
#[ignore]
async fn test_router_with_openrouter() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(provider()));
    router.set_default_provider("openrouter");

    let request = ChatRequest::new(
        "openrouter:openai/gpt-4o-mini",
        vec![Message::user("Reply with only the word 'pong'.")],
    )
    .with_temperature(0.0)
    .with_max_tokens(10);

    let response = router.chat_completion(request).await.unwrap();
    let text = response.first_text().unwrap().to_lowercase();
    assert!(
        text.contains("pong"),
        "Router response should contain 'pong', got: {text}"
    );
}

#[tokio::test]
#[ignore]
async fn test_router_model_alias() {
    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(provider()));
    router.add_model_mapping("cheap", "openrouter:openai/gpt-4o-mini");

    let request = ChatRequest::new(
        "cheap",
        vec![Message::user("What is 10 * 10? Reply with just the number.")],
    )
    .with_temperature(0.0)
    .with_max_tokens(10);

    let response = router.chat_completion(request).await.unwrap();
    let text = response.first_text().unwrap();
    assert!(
        text.contains("100"),
        "Aliased model should work, got: {text}"
    );
}
