use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Content part for multimodal messages (vision support).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { url: String, detail: Option<String> },
}

/// Cache control directive for prompt caching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

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
    /// Multimodal content parts (vision). If non-empty, takes priority over `content`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_parts: Vec<ChatContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
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
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
}

/// Log entry for a single LLM call, passed to the log callback.
#[derive(Debug, Clone)]
pub struct LlmCallLog {
    /// The full request sent to the LLM.
    pub request: ChatRequestSimple,
    /// The response from the LLM (None if an error occurred).
    pub response: Option<ChatResponseSimple>,
    /// Error message string if the LLM call failed.
    pub error_str: Option<String>,
    /// Latency of the LLM call in milliseconds.
    pub latency_ms: i64,
    /// RFC3339 timestamp (millisecond precision) of when the request was sent.
    pub requested_at: String,
    /// Whether this is a bot-internal loop iteration (tool call follow-up), i.e., iteration > 1.
    pub is_bot_iteration: bool,
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
