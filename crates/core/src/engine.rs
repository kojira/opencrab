use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing;

// ---------------------------------------------------------------------------
// Trait: ActionExecutor
// ---------------------------------------------------------------------------

/// Result of executing an action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// Whether the action succeeded.
    pub success: bool,
    /// The result data (format depends on the action).
    pub data: Value,
    /// Optional error message if the action failed.
    pub error: Option<String>,
}

/// Trait for executing actions (tool calls).
///
/// This trait is defined in `opencrab-core` so that the engine can call
/// actions without depending on `opencrab-actions` directly. The actions
/// crate implements this trait.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Execute an action by name with the given arguments.
    async fn execute(&self, name: &str, args: &Value) -> ActionResult;

    /// List available action (tool) definitions for LLM function calling.
    fn list_tools(&self) -> Vec<ToolDefinition>;
}

// ---------------------------------------------------------------------------
// Trait: LlmClient
// ---------------------------------------------------------------------------

/// A simplified chat message for the engine's LLM interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role: "system", "user", "assistant", or "tool".
    pub role: String,
    /// Text content.
    pub content: String,
    /// Tool call results (only for role = "tool").
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant (only for role = "assistant").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

/// A tool/function definition for LLM function calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// The name of the tool/function.
    pub name: String,
    /// Description of what the tool does.
    pub description: String,
    /// JSON Schema describing the parameters.
    pub parameters: Value,
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique ID for this tool call (used to match results).
    pub id: String,
    /// The name of the function to call.
    pub name: String,
    /// The arguments to pass (as a JSON object).
    pub arguments: Value,
}

/// A simplified chat request for the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequestSimple {
    /// The model to use (provider-specific identifier).
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<ChatMessage>,
    /// Available tools for function calling.
    pub tools: Vec<ToolDefinition>,
    /// Temperature for generation (0.0 to 2.0).
    pub temperature: Option<f32>,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
}

/// A simplified chat response from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponseSimple {
    /// Text content in the response (may be empty if only tool calls).
    pub content: Option<String>,
    /// Tool calls the LLM wants to make.
    pub tool_calls: Vec<ToolCall>,
    /// Whether the response is complete or was truncated.
    pub finish_reason: String,
    /// Token usage information.
    pub usage: Option<UsageInfo>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Trait for LLM chat completion.
///
/// Defined in `opencrab-core` so the engine can call the LLM without
/// depending on `opencrab-llm` directly. The LLM crate implements this trait.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat request and receive a response.
    async fn chat(&self, request: ChatRequestSimple) -> Result<ChatResponseSimple>;
}

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
        let tools = self.executor.list_tools();

        let mut messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_context.to_string(),
                tool_call_id: None,
                tool_calls: vec![],
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_message.to_string(),
                tool_call_id: None,
                tool_calls: vec![],
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

            tracing::debug!(iteration = iterations, "SkillEngine LLM call");

            let request = ChatRequestSimple {
                model: model.to_string(),
                messages: messages.clone(),
                tools: tools.clone(),
                temperature: Some(0.7),
                max_tokens: Some(4096),
            };

            let response = self.llm.chat(request).await?;

            // If there are tool calls, execute them and continue the loop.
            if !response.tool_calls.is_empty() {
                // Add the assistant message with tool calls.
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response.content.clone().unwrap_or_default(),
                    tool_call_id: None,
                    tool_calls: response.tool_calls.clone(),
                });

                for tool_call in &response.tool_calls {
                    total_tool_calls += 1;

                    tracing::debug!(
                        tool = %tool_call.name,
                        id = %tool_call.id,
                        "Executing tool call"
                    );

                    let result = self.executor.execute(&tool_call.name, &tool_call.arguments).await;

                    let result_json = serde_json::to_string(&result)
                        .unwrap_or_else(|_| r#"{"error": "Failed to serialize result"}"#.to_string());

                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: result_json,
                        tool_call_id: Some(tool_call.id.clone()),
                        tool_calls: vec![],
                    });
                }

                continue;
            }

            // No tool calls: this is the final response.
            let final_text = response
                .content
                .unwrap_or_else(|| "(No response generated)".to_string());

            return Ok(EngineResult {
                response: final_text,
                iterations,
                tool_calls_made: total_tool_calls,
                stopped_by_limit: false,
            });
        }
    }
}

/// The result of an engine run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineResult {
    /// The final text response.
    pub response: String,
    /// How many LLM call iterations were performed.
    pub iterations: usize,
    /// Total number of tool calls executed.
    pub tool_calls_made: usize,
    /// Whether the engine stopped due to hitting the iteration limit.
    pub stopped_by_limit: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

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
            self.results
                .get(name)
                .cloned()
                .unwrap_or(ActionResult {
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

        let result = engine.run("system", "do something", "test-model").await.unwrap();
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

        let result = engine.run("system", "loop forever", "test-model").await.unwrap();
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

        let result = engine.run("system", "do two things", "test-model").await.unwrap();
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

        let result = engine.run("system", "query something", "test-model").await.unwrap();
        assert_eq!(result.response, "Received tool feedback");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }
}
