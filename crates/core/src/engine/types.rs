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

/// 1 バッチ（同一 assistant メッセージの `tool_calls`）をバックグラウンド subtask として
/// 起動したときのマーカー。
///
/// エンジンはこれを `{status:"spawned", subtask_id, tool, label}` の tool_result へ
/// 写して**同ターン**でエージェントへ返す。実処理は完了時に
/// `SubtaskCompletionSink` 経由で親セッションへ再注入される（RFC §1.3）。
///
/// バッチが複数ツールを含む場合も **subtask は 1 本**であり（順序保証と resume 1 回化。
/// `ToolDispatcher::dispatch_batch` 参照）、各 tool_call には同じ `subtask_id` が返る。
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// 起動した subtask の ID。
    pub subtask_id: String,
    /// 人間可読ラベル（`tool(主要引数)`。複数ツールなら `,` 連結）。
    pub label: String,
}

/// `dispatch_batch` に渡す 1 ツール呼び出し分の入力。
#[derive(Debug, Clone)]
pub struct DispatchCall {
    /// ツール名。
    pub tool_name: String,
    /// ツール引数（パース済み JSON）。
    pub args: Value,
    /// LLM が付けた tool_call id（executor へそのまま渡す）。
    pub tool_call_id: String,
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
    /// （discord_send 等）、および run 内共有状態を書くツール（select_llm）は
    /// `false`（＝従来どおり同期実行）を返すことを想定する。
    fn should_dispatch(&self, tool_name: &str) -> bool;

    /// **同一バッチのツール呼び出し群**を 1 本の background subtask として起動し、
    /// 同期的にマーカーを返す。
    ///
    /// バッチを 1 本にまとめるのは順序保証のためである。tool_call ごとに別タスクを
    /// spawn すると `write_file` → `execute_shell("cargo build")` のような依存順が
    /// 崩れる（速い方が先に完走する）。1 subtask 内で `calls` の順に逐次実行すれば、
    /// LLM が並べた順序がそのまま保たれ、完了通知（＝親の resume）も 1 回で済む。
    ///
    /// 実処理（`executor.execute_with_id`）は別タスクで走り、完了で
    /// `settle_completed`（DB 永続化 → sink 発火）が親セッションを resume させる。
    fn dispatch_batch(&self, calls: &[DispatchCall]) -> DispatchOutcome;
}

// ---------------------------------------------------------------------------
// Trait: LiveInboundSource (#289 — 走行中ターンへの新着注入)
// ---------------------------------------------------------------------------

/// 走行中のターンに届いた**新着のユーザー発言**を取りに行くフック（#289）。
///
/// 会話履歴はターン開始時に 1 度だけ組まれるため、ツール往復が長引くと、その間に
/// 届いた発言は次ターンまでエンジンから見えない。実測では「やめて」が 9 秒、
/// #284 の例では約 1 分遅れた。エンジンはツール往復のたびにこのフックを引き、
/// 返ってきた本文を次の LLM 呼び出しの入力へ **user メッセージとして足す**。
///
/// - **足すだけで、応答は強制しない。** 見てどうするか（答える／作業を続ける／
///   中断する）はエージェントの判断に委ねる（#288 の強制を撤回した方針）。
/// - **重複注入の防止は実装側の責務。** 「前回 poll 以降の分だけ」を返すこと
///   （取得済みの最大 id 等を実装が保持する）。同じ発言を毎イテレーション返すと
///   プロンプトが際限なく膨らむ。
/// - 履歴**全体**の組み直しはしない。重いうえにコンテキストが膨らむため、
///   差分だけを返す軽いクエリで実装すること。
/// - エンジンは core にあり DB を触れないので、実体（DB クエリ）は server 側に置き
///   `Arc<dyn LiveInboundSource>` として注入する。
pub trait LiveInboundSource: Send + Sync {
    /// 前回の呼び出し以降に届いた自分宛の新着発言を、古い順に整形して返す。
    ///
    /// 新着が無ければ空 Vec（＝プロンプトは 1 バイトも変わらない）。取得に失敗した
    /// 場合も空 Vec を返し、ターンは続行する（best-effort）。
    fn poll_new_messages(&self) -> Vec<String>;
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
