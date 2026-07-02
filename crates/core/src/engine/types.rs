use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Canonical LLM message model shared with the provider/router layer.
pub use opencrab_llm_types::{ChatRequest, ChatResponse, FunctionDefinition, ToolCall};

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

    /// Execute an action, propagating the LLM-provided `tool_call_id` for
    /// correlation (e.g. activity webhooks, tracing).
    ///
    /// The default implementation ignores the id and delegates to [`execute`],
    /// so existing implementors keep working unchanged. Implementors that emit
    /// observability events (like `BridgedExecutor`) override this to thread the
    /// real tool-call id instead of synthesizing one.
    async fn execute_with_id(&self, name: &str, args: &Value, _tool_call_id: &str) -> ActionResult {
        self.execute(name, args).await
    }

    /// List available action (tool) definitions for LLM function calling.
    fn list_tools(&self) -> Vec<FunctionDefinition>;
}

// ---------------------------------------------------------------------------
// Trait: LlmClient
// ---------------------------------------------------------------------------

/// Log entry for a single LLM call, passed to the log callback.
#[derive(Debug, Clone)]
pub struct LlmCallLog {
    /// The full request sent to the LLM.
    pub request: ChatRequest,
    /// The response from the LLM (None if an error occurred).
    pub response: Option<ChatResponse>,
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
/// depending on `opencrab-llm` (providers/router) directly. The server's
/// router adapter and test mocks implement this trait over the canonical
/// message model.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat request and receive a response.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
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
    /// XML `<function_calls>` フォールバックで tool calls を復元した回数。
    /// harness 剪定の判断材料（native tool calling で不要になれば 0 になる）。
    #[serde(default)]
    pub xml_fallback_parses: usize,
}
