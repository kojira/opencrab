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
// Trait: ToolDispatcher (RFC #152 S3a — 非ブロック / 全ツール自動 subtask 化)
// ---------------------------------------------------------------------------

/// 単一ツールをバックグラウンド subtask として起動したときのマーカー。
///
/// エンジンはこれを `{status:"spawned", subtask_id, tool, label}` の tool_result へ
/// 写して**同ターン**でエージェントへ返す。実処理は完了時に
/// `SubtaskCompletionSink` 経由で親セッションへ再注入される（RFC §1.3）。
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// 起動した subtask の ID。
    pub subtask_id: String,
    /// 人間可読ラベル（`tool(主要引数)`）。
    pub label: String,
}

/// エンジンのツール実行点で「auto-dispatch 対象ツールを background subtask 化」する
/// ためのフック（gateway 非依存）。
///
/// `SkillEngine` は `opencrab-actions` に依存できない（actions が core に依存する
/// 逆向き）ため、フックの trait を core に置き、実装（executor / registry / sink を
/// 保持する `SubtaskToolDispatcher`）は actions 側に置く。エンジンは
/// `Arc<dyn ToolDispatcher>` を保持し、ツール呼び出しごとに問い合わせるだけ。
pub trait ToolDispatcher: Send + Sync {
    /// このツールを background subtask として dispatch すべきか。
    ///
    /// 制御系（spawn_subtask / cancel_subtask / report_progress）や配送系
    /// （discord_send 等）は `false`（＝従来どおり同期実行）を返すことを想定する。
    fn should_dispatch(&self, tool_name: &str) -> bool;

    /// ツールを background subtask として起動し、**同期的に**マーカーを返す。
    ///
    /// 実処理（`executor.execute_with_id`）は別タスクで走り、完了で
    /// `settle_completed`（DB 永続化 → sink 発火）が親セッションを resume させる。
    fn dispatch(
        &self,
        tool_name: &str,
        args: &Value,
        tool_call_id: &str,
    ) -> DispatchOutcome;
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
