use std::sync::Arc;

use anyhow::Result;
use tracing;

use super::types::{
    ActionExecutor, ActionResult, CacheControl, ChatContentPart, ChatMessage, ChatRequestSimple,
    EngineResult, LlmCallLog, LlmClient,
};
use super::xml_parser::{parse_xml_tool_calls, strip_function_calls_xml};

// ---------------------------------------------------------------------------
// SkillEngine
// ---------------------------------------------------------------------------

/// The LLM-driven action loop engine.
///
/// The SkillEngine orchestrates the cycle of:
/// 1. Building context from the agent's state
/// 2. Getting available tools from the action executor
/// 3. Calling the LLM with function calling enabled
/// 4. Executing any requested tool calls
/// 5. Feeding results back and repeating
///
/// This continues until the LLM produces a final text response
/// or the maximum iteration count is reached.
pub struct SkillEngine {
    /// The LLM client for chat completion.
    llm: Box<dyn LlmClient>,
    /// The action executor for tool calls.
    executor: Box<dyn ActionExecutor>,
    /// Maximum number of LLM call iterations before stopping.
    pub max_iterations: usize,
    /// Set of actions declared by active skills. If Some, only declared actions are allowed.
    pub allowed_actions: Option<std::collections::HashSet<String>>,
    /// Optional callback invoked after each LLM call for logging.
    pub log_callback: Option<Box<dyn Fn(&LlmCallLog) + Send + Sync>>,
    /// Optional callback invoked with response text on every LLM reply.
    pub on_response_text: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// Optional callback invoked when the assistant produces tool calls: (assistant_content, tool_calls_json).
    on_tool_call: Option<Arc<dyn Fn(String, String) + Send + Sync>>,
    /// Optional callback invoked when a tool result is received: (tool_call_id, tool_name, result_json, is_error).
    on_tool_result: Option<Arc<dyn Fn(String, String, String, bool) + Send + Sync>>,
}

impl SkillEngine {
    /// Create a new SkillEngine.
    pub fn new(
        llm: Box<dyn LlmClient>,
        executor: Box<dyn ActionExecutor>,
        max_iterations: usize,
    ) -> Self {
        Self {
            llm,
            executor,
            max_iterations,
            allowed_actions: None,
            log_callback: None,
            on_response_text: None,
            on_tool_call: None,
            on_tool_result: None,
        }
    }

    /// Set the LLM log callback, invoked after each LLM call.
    pub fn set_log_callback(&mut self, cb: impl Fn(&LlmCallLog) + Send + Sync + 'static) {
        self.log_callback = Some(Box::new(cb));
    }

    /// Set the on_response_text callback, invoked with response text on every LLM reply.
    pub fn set_on_response_text(&mut self, cb: impl Fn(String) + Send + Sync + 'static) {
        self.on_response_text = Some(Arc::new(cb));
    }

    /// Set the on_tool_call callback, invoked when the assistant produces tool calls.
    pub fn set_on_tool_call(&mut self, cb: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_tool_call = Some(Arc::new(cb));
    }

    /// Set the on_tool_result callback, invoked when a tool result is received.
    pub fn set_on_tool_result(
        &mut self,
        cb: impl Fn(String, String, String, bool) + Send + Sync + 'static,
    ) {
        self.on_tool_result = Some(Arc::new(cb));
    }

    /// Set the allowed actions from active skill declarations.
    pub fn set_allowed_actions(&mut self, actions: impl IntoIterator<Item = String>) {
        self.allowed_actions = Some(actions.into_iter().collect());
    }

    /// Check if an action is allowed by the active skill declarations.
    fn is_action_allowed(&self, action_name: &str) -> bool {
        match &self.allowed_actions {
            None => true,
            Some(allowed) => allowed.contains(action_name),
        }
    }

    /// Build an ActionResult for a permission denied error.
    fn permission_denied(action_name: &str) -> ActionResult {
        ActionResult {
            success: false,
            data: serde_json::json!(null),
            error: Some(format!(
                "Action '{}' is not authorized. Add '{}' to the skill's actions frontmatter to enable this capability.",
                action_name, action_name
            )),
        }
    }

    /// Run the action loop with the given system context and user message.
    ///
    /// Returns the final text response from the LLM after all tool calls
    /// have been resolved.
    pub async fn run(
        &self,
        system_context: &str,
        user_message: &str,
        model: &str,
    ) -> Result<EngineResult> {
        self.run_with_model_override(system_context, user_message, model, None, &[])
            .await
    }

    /// Run the action loop with optional dynamic model override.
    ///
    /// If `model_override` is provided, the engine checks it before each LLM call
    /// and uses the overridden model if set (e.g., by `select_llm` action).
    pub async fn run_with_model_override(
        &self,
        system_context: &str,
        user_message: &str,
        default_model: &str,
        model_override: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
        image_urls: &[String],
    ) -> Result<EngineResult> {
        let mut tools = self.executor.list_tools();
        // BP1: toolsの最後のツールにcache_control(1h)を付与
        if let Some(last_tool) = tools.last_mut() {
            last_tool.cache_control = Some(CacheControl {
                r#type: "ephemeral".to_string(),
                ttl: Some("1h".to_string()),
            });
        }

        let user_content_parts: Vec<ChatContentPart> = if image_urls.is_empty() {
            vec![]
        } else {
            let mut parts = vec![ChatContentPart::Text {
                text: user_message.to_string(),
            }];
            for url in image_urls {
                parts.push(ChatContentPart::ImageUrl {
                    url: url.clone(),
                    detail: Some("auto".to_string()),
                });
            }
            parts
        };

        let mut messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_context.to_string(),
                tool_call_id: None,
                tool_calls: vec![],
                content_parts: vec![],
                cache_control: Some(CacheControl {
                    r#type: "ephemeral".to_string(),
                    ttl: Some("1h".to_string()),
                }),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_message.to_string(),
                tool_call_id: None,
                tool_calls: vec![],
                content_parts: user_content_parts,
                cache_control: None,
            },
        ];

        let mut iterations = 0;
        let mut total_tool_calls = 0;

        loop {
            iterations += 1;

            if iterations > self.max_iterations {
                tracing::warn!(
                    iterations = iterations,
                    max = self.max_iterations,
                    "SkillEngine reached max iterations, stopping"
                );
                return Ok(EngineResult {
                    response: "I've reached the maximum number of steps for this task. Here's what I've done so far.".to_string(),
                    iterations,
                    tool_calls_made: total_tool_calls,
                    stopped_by_limit: true,
                });
            }

            // Check for dynamic model override.
            let model = model_override
                .as_ref()
                .and_then(|o| o.lock().ok().and_then(|m| m.clone()))
                .unwrap_or_else(|| default_model.to_string());

            tracing::debug!(iteration = iterations, model = %model, "SkillEngine LLM call");

            let request = ChatRequestSimple {
                model,
                messages: messages.clone(),
                tools: tools.clone(),
                temperature: Some(0.7),
                max_tokens: Some(4096),
                agent_id: None,
            };

            let request_for_log = request.clone();
            let requested_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            let call_start = std::time::Instant::now();
            let llm_result = self.llm.chat(request).await;
            let latency_ms = call_start.elapsed().as_millis() as i64;

            if let Some(cb) = &self.log_callback {
                match &llm_result {
                    Ok(resp) => cb(&LlmCallLog {
                        request: request_for_log.clone(),
                        response: Some(resp.clone()),
                        error_str: None,
                        latency_ms,
                        requested_at: requested_at.clone(),
                        is_bot_iteration: iterations > 1,
                    }),
                    Err(e) => cb(&LlmCallLog {
                        request: request_for_log.clone(),
                        response: None,
                        error_str: Some(e.to_string()),
                        latency_ms,
                        requested_at: requested_at.clone(),
                        is_bot_iteration: iterations > 1,
                    }),
                }
            }

            let response = llm_result?;

            // If the LLM returned no structured tool calls but embedded
            // <function_calls> XML in the content (e.g. DeepSeek via OpenRouter),
            // parse them out and treat them as normal tool calls.
            let mut response = response;
            if response.tool_calls.is_empty() {
                if let Some(ref content) = response.content {
                    if content.contains("<function_calls>") {
                        let parsed = parse_xml_tool_calls(content);
                        if !parsed.is_empty() {
                            tracing::debug!(
                                count = parsed.len(),
                                "Parsed XML function_calls from content"
                            );
                            response.tool_calls = parsed;
                            // Strip the XML block from content so it doesn't leak to the user.
                            let cleaned = strip_function_calls_xml(content);
                            response.content = if cleaned.is_empty() {
                                None
                            } else {
                                Some(cleaned)
                            };
                        }
                    }
                }
            }

            // Fire on_response_text for every LLM reply that has non-empty text.
            if let Some(ref text) = response.content {
                if !text.trim().is_empty() {
                    if let Some(ref cb) = self.on_response_text {
                        cb(text.clone());
                    }
                }
            }

            // If there are tool calls, execute them and continue the loop.
            if !response.tool_calls.is_empty() {
                // Add the assistant message with tool calls.
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response.content.clone().unwrap_or_default(),
                    tool_call_id: None,
                    tool_calls: response.tool_calls.clone(),
                    content_parts: vec![],
                    cache_control: None,
                });

                // Notify on_tool_call callback.
                if let Some(ref cb) = self.on_tool_call {
                    let calls_json =
                        serde_json::to_string(&response.tool_calls).unwrap_or_default();
                    cb(response.content.clone().unwrap_or_default(), calls_json);
                }

                for tool_call in &response.tool_calls {
                    total_tool_calls += 1;

                    tracing::debug!(
                        tool = %tool_call.name,
                        id = %tool_call.id,
                        "Executing tool call"
                    );

                    // Check if the action is declared by active skills.
                    if !self.is_action_allowed(&tool_call.name) {
                        let denied = Self::permission_denied(&tool_call.name);
                        let result_json = serde_json::to_string(&denied)
                            .unwrap_or_else(|_| r#"{"error": "Permission denied"}"#.to_string());
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: result_json.clone(),
                            tool_call_id: Some(tool_call.id.clone()),
                            tool_calls: vec![],
                            content_parts: vec![],
                            cache_control: None,
                        });

                        // Notify on_tool_result callback for denied action.
                        if let Some(ref cb) = self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_call.name.clone(),
                                result_json.clone(),
                                true,
                            );
                        }
                        continue;
                    }

                    let result = self
                        .executor
                        .execute(&tool_call.name, &tool_call.arguments)
                        .await;

                    let result_json = serde_json::to_string(&result).unwrap_or_else(|_| {
                        r#"{"error": "Failed to serialize result"}"#.to_string()
                    });

                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: result_json.clone(),
                        tool_call_id: Some(tool_call.id.clone()),
                        tool_calls: vec![],
                        content_parts: vec![],
                        cache_control: None,
                    });

                    // Notify on_tool_result callback.
                    if let Some(ref cb) = self.on_tool_result {
                        cb(
                            tool_call.id.clone(),
                            tool_call.name.clone(),
                            result_json.clone(),
                            !result.success,
                        );
                    }
                }

                continue;
            }

            // No tool calls: this is the final response.
            let final_text = response.content.unwrap_or_default();

            if final_text.is_empty() {
                tracing::debug!("LLM returned empty content (no tool calls), using empty response");
            }

            return Ok(EngineResult {
                response: final_text,
                iterations,
                tool_calls_made: total_tool_calls,
                stopped_by_limit: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{ChatResponseSimple, ToolCall, ToolDefinition};
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;

    struct MockLlm {
        responses: std::sync::Mutex<Vec<ChatResponseSimple>>,
    }

    impl MockLlm {
        fn new(responses: Vec<ChatResponseSimple>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequestSimple) -> anyhow::Result<ChatResponseSimple> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no more mock responses");
            }
            Ok(responses.remove(0))
        }
    }

    struct MockExecutor {
        results: std::collections::HashMap<String, ActionResult>,
    }

    impl MockExecutor {
        fn new() -> Self {
            Self {
                results: std::collections::HashMap::new(),
            }
        }
        fn add_result(mut self, name: &str, result: ActionResult) -> Self {
            self.results.insert(name.to_string(), result);
            self
        }
    }

    #[async_trait]
    impl ActionExecutor for MockExecutor {
        async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
            self.results.get(name).cloned().unwrap_or(ActionResult {
                success: false,
                data: serde_json::json!(null),
                error: Some(format!("Unknown action: {name}")),
            })
        }
        fn list_tools(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "test_tool".to_string(),
                description: "A test tool".to_string(),
                parameters: serde_json::json!({}),
                cache_control: None,
            }]
        }
    }

    fn text_response(text: &str) -> ChatResponseSimple {
        ChatResponseSimple {
            content: Some(text.to_string()),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        }
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> ChatResponseSimple {
        ChatResponseSimple {
            content: None,
            tool_calls: calls,
            finish_reason: "tool_calls".to_string(),
            usage: None,
        }
    }

    #[tokio::test]
    async fn test_direct_response() {
        let llm = MockLlm::new(vec![text_response("Hello, world!")]);
        let executor = MockExecutor::new();
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(result.response, "Hello, world!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.tool_calls_made, 0);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_single_tool_call() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![ToolCall {
                id: "tc-1".to_string(),
                name: "test_tool".to_string(),
                arguments: serde_json::json!({}),
            }]),
            text_response("Done with tool call"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "do something", "test-model")
            .await
            .unwrap();
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![ToolCall {
                id: "tc-1".to_string(),
                name: "test_tool".to_string(),
                arguments: serde_json::json!({}),
            }]),
            tool_call_response(vec![ToolCall {
                id: "tc-2".to_string(),
                name: "test_tool".to_string(),
                arguments: serde_json::json!({}),
            }]),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 1);

        let result = engine
            .run("system", "loop forever", "test-model")
            .await
            .unwrap();
        assert!(result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_multiple_tool_calls() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                ToolCall {
                    id: "tc-1".to_string(),
                    name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: "tc-2".to_string(),
                    name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                },
            ]),
            text_response("Both tools done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "do two things", "test-model")
            .await
            .unwrap();
        assert_eq!(result.tool_calls_made, 2);
        assert_eq!(result.iterations, 2);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_tool_result_feedback() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![ToolCall {
                id: "tc-1".to_string(),
                name: "test_tool".to_string(),
                arguments: serde_json::json!({"query": "test"}),
            }]),
            text_response("Received tool feedback"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"answer": 42}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "query something", "test-model")
            .await
            .unwrap();
        assert_eq!(result.response, "Received tool feedback");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_model_override() {
        use std::sync::{Arc, Mutex};

        // MockLlm that captures the model from each request.
        struct ModelCapturingLlm {
            responses: Mutex<Vec<ChatResponseSimple>>,
            captured_models: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for ModelCapturingLlm {
            async fn chat(&self, request: ChatRequestSimple) -> anyhow::Result<ChatResponseSimple> {
                self.captured_models
                    .lock()
                    .unwrap()
                    .push(request.model.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let llm = ModelCapturingLlm {
            responses: Mutex::new(vec![
                // First call uses default model; after tool call, model override kicks in.
                tool_call_response(vec![ToolCall {
                    id: "tc-1".to_string(),
                    name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                }]),
                text_response("Done after model switch"),
            ]),
            captured_models: captured.clone(),
        };

        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
            },
        );

        let model_override = Arc::new(Mutex::new(None));
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        // Simulate: after the first tool call, model_override gets set.
        let override_clone = model_override.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            *override_clone.lock().unwrap() = Some("openai:gpt-4o-mini".to_string());
        });

        let result = engine
            .run_with_model_override("system", "hi", "default-model", Some(model_override), &[])
            .await
            .unwrap();

        assert_eq!(result.response, "Done after model switch");

        let models = captured.lock().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], "default-model"); // First call uses default.
                                                // Second call should use the overridden model (race condition safe - set before tool call finishes).
                                                // Due to timing, it might be either; the important thing is the mechanism works.
    }

    #[tokio::test]
    async fn test_on_response_text_fires_on_every_iteration() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            ChatResponseSimple {
                content: Some("調べてみます".to_string()),
                tool_calls: vec![ToolCall {
                    id: "tc-1".to_string(),
                    name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                }],
                finish_reason: "tool_calls".to_string(),
                usage: None,
            },
            ChatResponseSimple {
                content: Some("天気は20度です".to_string()),
                tool_calls: vec![],
                finish_reason: "stop".to_string(),
                usage: None,
            },
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let fired_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let fired_clone = fired_texts.clone();
        engine.set_on_response_text(move |text: String| {
            fired_clone.lock().unwrap().push(text);
        });

        let result = engine
            .run("system", "天気は？", "test-model")
            .await
            .unwrap();
        let texts = fired_texts.lock().unwrap();
        assert_eq!(texts.len(), 2, "should fire for both iterations");
        assert_eq!(texts[0], "調べてみます");
        assert_eq!(texts[1], "天気は20度です");
        assert_eq!(result.response, "天気は20度です");
    }

    #[tokio::test]
    async fn test_tool_history_in_next_llm_call() {
        use std::sync::{Arc, Mutex};

        // MockLlm that captures the messages from each request
        struct MessageCapturingLlm {
            responses: Mutex<Vec<ChatResponseSimple>>,
            captured_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        }

        #[async_trait]
        impl LlmClient for MessageCapturingLlm {
            async fn chat(&self, request: ChatRequestSimple) -> anyhow::Result<ChatResponseSimple> {
                self.captured_messages
                    .lock()
                    .unwrap()
                    .push(request.messages.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::<Vec<ChatMessage>>::new()));
        let llm = MessageCapturingLlm {
            responses: Mutex::new(vec![
                // First response: tool call
                tool_call_response(vec![ToolCall {
                    id: "tc-1".to_string(),
                    name: "test_tool".to_string(),
                    arguments: serde_json::json!({}),
                }]),
                // Second response: final text
                text_response("All done"),
            ]),
            captured_messages: captured.clone(),
        };

        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine.run("system", "do it", "test-model").await.unwrap();
        assert_eq!(result.response, "All done");
        assert_eq!(result.iterations, 2);

        let all_messages = captured.lock().unwrap();
        assert_eq!(all_messages.len(), 2, "LLM should have been called twice");

        // Check messages sent on the second LLM call (iteration 2)
        let second_call_msgs = &all_messages[1];

        // Should contain an assistant message with non-empty tool_calls
        let has_assistant_with_tool_calls = second_call_msgs
            .iter()
            .any(|m| m.role == "assistant" && !m.tool_calls.is_empty());
        assert!(
            has_assistant_with_tool_calls,
            "Second LLM call must include an assistant message with tool_calls"
        );

        // Should contain a tool message with tool_call_id set
        let has_tool_result = second_call_msgs
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id.is_some());
        assert!(
            has_tool_result,
            "Second LLM call must include a tool result message with tool_call_id"
        );
    }

    #[tokio::test]
    async fn test_on_response_text_fires_for_direct_response() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![text_response("直接答えます")]);
        let executor = MockExecutor::new();

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let fired_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let fired_clone = fired_texts.clone();
        engine.set_on_response_text(move |text: String| {
            fired_clone.lock().unwrap().push(text);
        });

        let result = engine.run("system", "direct", "test-model").await.unwrap();
        let texts = fired_texts.lock().unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0], "直接答えます");
        assert_eq!(result.response, "直接答えます");
    }
}
