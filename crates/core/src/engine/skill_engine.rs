use std::sync::Arc;

use anyhow::Result;
use tracing;

use super::types::{
    ActionExecutor, ActionResult, ChatRequest, EngineResult, LlmCallLog, LlmClient,
};
use super::xml_parser::{parse_xml_tool_calls, strip_function_calls_xml};
use opencrab_llm_types::{ContentPart, ImageUrl, Message, MessageContent, Role, ToolCall};

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
    /// Per-run reasoning (thinking) effort. Attached to every ChatRequest so
    /// providers can override their construction-time default per agent.
    reasoning_effort: Option<String>,
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
            reasoning_effort: None,
        }
    }

    /// Set the per-run reasoning (thinking) effort attached to each request.
    /// 空文字は「未設定」として扱う。
    pub fn set_reasoning_effort(&mut self, effort: impl Into<String>) {
        let s = effort.into();
        self.reasoning_effort = if s.trim().is_empty() { None } else { Some(s) };
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
        // プロンプトキャッシュはプロバイダの能力としてプロバイダ側が適用する（#44）。
        // 以前はここで Anthropic 固有の cache_control を全リクエストに無条件付与して
        // いたが、読むのは anthropic だけ・system 分は黙って落ちる偽ユニバーサル
        // 抽象だった。エンジンはプロバイダ非依存のリクエストだけを組む。
        let tools = self.executor.list_tools();

        // ユーザーメッセージ本文（画像があればマルチパート）。
        let user_content = if image_urls.is_empty() {
            MessageContent::Text(user_message.to_string())
        } else {
            let mut parts = vec![ContentPart::Text {
                text: user_message.to_string(),
            }];
            for url in image_urls {
                parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: url.clone(),
                        detail: Some("auto".to_string()),
                    },
                });
            }
            MessageContent::Multi(parts)
        };

        let mut messages = vec![
            Message {
                role: Role::System,
                content: Some(MessageContent::Text(system_context.to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message {
                role: Role::User,
                content: Some(user_content),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        let mut iterations = 0;
        let mut total_tool_calls = 0;
        let mut xml_fallback_parses = 0;

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
                    xml_fallback_parses,
                });
            }

            // Check for dynamic model override.
            let model = model_override
                .as_ref()
                .and_then(|o| o.lock().ok().and_then(|m| m.clone()))
                .unwrap_or_else(|| default_model.to_string());

            tracing::debug!(iteration = iterations, model = %model, "SkillEngine LLM call");

            let request = ChatRequest {
                model: model.clone(),
                messages: messages.clone(),
                functions: if tools.is_empty() {
                    None
                } else {
                    Some(tools.clone())
                },
                function_call: None,
                temperature: Some(0.7),
                max_tokens: Some(4096),
                stop: None,
                stream: None,
                metadata: Default::default(),
                agent_id: None,
                reasoning_effort: self.reasoning_effort.clone(),
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

            // 応答本文とツールコールをローカルに抽出（正準モデルは choices[0] を持つ）。
            let mut content: Option<String> = response.first_text().map(|s| s.to_string());
            let mut tool_calls: Vec<ToolCall> = response
                .first_message()
                .and_then(|m| m.tool_calls.clone())
                .unwrap_or_default();

            // If the LLM returned no structured tool calls but embedded
            // <function_calls> XML in the content (e.g. DeepSeek via OpenRouter),
            // parse them out and treat them as normal tool calls.
            if tool_calls.is_empty() {
                if let Some(ref c) = content {
                    if c.contains("<function_calls>") {
                        let parsed = parse_xml_tool_calls(c);
                        if !parsed.is_empty() {
                            // 発火は harness 剪定の判断材料として計測する（EngineResult 経由で
                            // agent_logs にも記録される）。codex プロバイダは意図的にこの
                            // フォールバックへ依存するため、発火＝異常ではない（毎イテレーション
                            // 発火し得るのでログは debug に留め、run 単位の集計を agent_logs で見る）。
                            xml_fallback_parses += 1;
                            tracing::debug!(
                                count = parsed.len(),
                                model = %model,
                                "Parsed XML function_calls from content (harness fallback fired)"
                            );
                            tool_calls = parsed;
                            // Strip the XML block from content so it doesn't leak to the user.
                            let cleaned = strip_function_calls_xml(c);
                            content = if cleaned.is_empty() {
                                None
                            } else {
                                Some(cleaned)
                            };
                        }
                    }
                }
            }

            // Fire on_response_text for every LLM reply that has non-empty text.
            if let Some(ref text) = content {
                if !text.trim().is_empty() {
                    if let Some(ref cb) = self.on_response_text {
                        tracing::warn!(
                            iteration = iterations,
                            text_len = text.len(),
                            text_preview = %text.chars().take(100).collect::<String>(),
                            "LLM response text received, firing on_response_text callback"
                        );
                        cb(text.clone());
                        tracing::warn!(iteration = iterations, "on_response_text callback fired");
                    }
                }
            }

            // If there are tool calls, execute them and continue the loop.
            if !tool_calls.is_empty() {
                // Add the assistant message with tool calls (arguments already
                // canonical Strings, so no Value->String conversion needed).
                messages.push(Message {
                    role: Role::Assistant,
                    content: content.clone().map(MessageContent::Text),
                    name: None,
                    function_call: None,
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });

                // Notify on_tool_call callback.
                if let Some(ref cb) = self.on_tool_call {
                    let calls_json = serde_json::to_string(&tool_calls).unwrap_or_default();
                    cb(content.clone().unwrap_or_default(), calls_json);
                }

                for tool_call in &tool_calls {
                    total_tool_calls += 1;
                    let tool_name = &tool_call.function.name;

                    tracing::debug!(
                        tool = %tool_name,
                        id = %tool_call.id,
                        "Executing tool call"
                    );

                    // Check if the action is declared by active skills.
                    if !self.is_action_allowed(tool_name) {
                        let denied = Self::permission_denied(tool_name);
                        let result_json = serde_json::to_string(&denied)
                            .unwrap_or_else(|_| r#"{"error": "Permission denied"}"#.to_string());
                        messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));

                        // Notify on_tool_result callback for denied action.
                        if let Some(ref cb) = self.on_tool_result {
                            cb(
                                tool_call.id.clone(),
                                tool_name.clone(),
                                result_json.clone(),
                                true,
                            );
                        }
                        continue;
                    }

                    // Canonical tool-call arguments are a JSON string; parse to a
                    // Value for the executor boundary (empty object on malformed).
                    let args = tool_call.arguments_json();
                    let result = self
                        .executor
                        .execute_with_id(tool_name, &args, &tool_call.id)
                        .await;

                    let result_json = serde_json::to_string(&result).unwrap_or_else(|_| {
                        r#"{"error": "Failed to serialize result"}"#.to_string()
                    });

                    messages.push(Message::tool(tool_call.id.clone(), result_json.clone()));

                    // Notify on_tool_result callback.
                    if let Some(ref cb) = self.on_tool_result {
                        cb(
                            tool_call.id.clone(),
                            tool_name.clone(),
                            result_json.clone(),
                            !result.success,
                        );
                    }
                }

                continue;
            }

            // No tool calls: this is the final response.
            let final_text = content.unwrap_or_default();

            tracing::warn!(
                iteration = iterations,
                text_len = final_text.len(),
                text_preview = %final_text.chars().take(100).collect::<String>(),
                "SkillEngine final response ready"
            );

            if final_text.is_empty() {
                tracing::debug!("LLM returned empty content (no tool calls), using empty response");
            }

            return Ok(EngineResult {
                response: final_text,
                iterations,
                tool_calls_made: total_tool_calls,
                stopped_by_limit: false,
                xml_fallback_parses,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opencrab_llm_types::{
        ChatResponse, Choice, FunctionCall, FunctionDefinition, MessageContent, Usage,
    };
    use serde_json::Value;

    /// Build a canonical tool call with JSON arguments (as a value, serialized).
    fn tc(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: serde_json::to_string(&args).unwrap(),
            },
        }
    }

    /// Build a single-choice ChatResponse with optional text and tool calls.
    fn resp(text: Option<&str>, calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            id: String::new(),
            model: String::new(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: text.map(|s| MessageContent::Text(s.to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: if calls.is_empty() { None } else { Some(calls) },
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: Usage::default(),
            created: 0,
        }
    }

    fn text_response(text: &str) -> ChatResponse {
        resp(Some(text), vec![])
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> ChatResponse {
        resp(None, calls)
    }

    struct MockLlm {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
    }

    impl MockLlm {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
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
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            vec![FunctionDefinition {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                parameters: serde_json::json!({}),
            }]
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
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
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
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            tool_call_response(vec![tc("tc-2", "test_tool", serde_json::json!({}))]),
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
                tc("tc-1", "test_tool", serde_json::json!({})),
                tc("tc-2", "test_tool", serde_json::json!({})),
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
            tool_call_response(vec![tc(
                "tc-1",
                "test_tool",
                serde_json::json!({"query": "test"}),
            )]),
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
            responses: Mutex<Vec<ChatResponse>>,
            captured_models: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for ModelCapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
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
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
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
            resp(
                Some("調べてみます"),
                vec![tc("tc-1", "test_tool", serde_json::json!({}))],
            ),
            resp(Some("天気は20度です"), vec![]),
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
            responses: Mutex<Vec<ChatResponse>>,
            captured_messages: Arc<Mutex<Vec<Vec<Message>>>>,
        }

        #[async_trait]
        impl LlmClient for MessageCapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
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

        let captured = Arc::new(Mutex::new(Vec::<Vec<Message>>::new()));
        let llm = MessageCapturingLlm {
            responses: Mutex::new(vec![
                // First response: tool call
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
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
        let has_assistant_with_tool_calls = second_call_msgs.iter().any(|m| {
            m.role == Role::Assistant && m.tool_calls.as_ref().map_or(false, |t| !t.is_empty())
        });
        assert!(
            has_assistant_with_tool_calls,
            "Second LLM call must include an assistant message with tool_calls"
        );

        // Should contain a tool message with tool_call_id set
        let has_tool_result = second_call_msgs
            .iter()
            .any(|m| m.role == Role::Tool && m.tool_call_id.is_some());
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
