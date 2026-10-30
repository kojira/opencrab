pub mod agent_runtime;
pub mod bridge;
pub mod common;
pub mod continue_marker;
pub mod dispatcher;
pub mod learning;
pub mod llm_analysis;
pub mod llm_evaluation;
pub mod llm_selection;
pub mod memory_access;
pub mod memory_units;
pub mod model_admin;
pub mod no_reply;
pub mod search;
pub mod session_runtime;
pub mod skill_management;
pub mod soul;
pub mod subtask;
pub mod subtask_registries;
pub mod task_ledger;
pub mod timed_fire;
pub mod tools;
pub mod traits;
pub mod transcript;
pub mod view_image;
pub mod workspace;

pub mod run_request;
pub mod session_inbound;

pub use agent_runtime::AgentRuntime;
pub use bridge::{
    tool_policy, BridgedExecutor, EffectiveToolDefinition, ExecutorRuntimeState,
    SubEngineGatewayActions, ToolEvent, ToolEventSink, ToolEventStatus, ToolPolicy, ToolSlot,
    CORE_DISPATCHABLE_ACTIONS, CORE_INLINE_ACTIONS, MCP_TOOL_PREFIX, OWNER_ONLY_ACTIONS,
    REJECTION_CODE_PREFIX, TRUSTED_ONLY_ACTIONS,
};
pub use continue_marker::visible_speech_after_markers;
pub use dispatcher::ActionDispatcher;
pub use model_admin::{
    EffortSnapshot, ModelAdminError, ModelAdministration, ModelSnapshot, SessionEffortList,
    SessionModelList, TurnModelProfile, REASONING_EFFORTS,
};
pub use no_reply::{
    terminate_at_no_reply, DeliveryContext, NoReplyTermination, NO_REPLY_LOG_TARGET,
    NO_REPLY_SENTINEL, NO_REPLY_TRAILING_DISCARDED_TAG,
};
pub use run_request::{
    LiveInboundScope, RunRequest, LIGHTWEIGHT_TURN_TOOLS, PROVISIONAL_READ_ONLY_TOOLS,
};
pub use session_inbound::{
    delivery_effect, prepare_session_inbound, prepare_session_inbound_write, run_provisional_turn,
    run_session_turn, start_session_turn, DeliveryEffect, NormalizedInbound,
    PrepareSessionInboundError,
};
pub use session_runtime::{SessionLocks, SessionRuntime};
pub use skill_management::OWNER_SKILLS_INDEX_NAME;
pub use subtask::{
    cancel_subtask, default_non_dispatch_tools, dispatch_settled, steer_subtask, CancelOutcome,
    NoopCompletionSink, SettleKind, SharedExecutor, SpawnedSubtask, SteerOutcome,
    SubtaskCompletionSink, SubtaskLifecycle, SubtaskLifecycleEvent, SubtaskLifecycleStatus,
    SubtaskRegistry, SubtaskSettled, SubtaskToolDispatcher, DEFAULT_DISPATCH_TIMEOUT_SECS,
    STEER_LOG_TYPE,
};
pub use subtask_registries::SubtaskRegistries;
pub use timed_fire::{
    new_turn_id, prompt_preview, FireTarget, TimedFireRequest, TimedFireRouter, TimedFireSink,
};
// tool_result の無害化は core 側（`opencrab_core::tool_result_log`）に一本化した
// （#284）。LLM へ返す経路（`SkillEngine`）と DB 永続化経路（server / dispatch）で
// **同一の上限と退避**を使う必要があり、core は actions に依存できないため。
// 既存の呼び出し元互換のためここから re-export する。
pub use opencrab_core::tool_result_log::{
    sanitize_tool_result_for_llm, sanitize_tool_result_for_log, TOOL_RESULT_TOKEN_LIMIT,
};
pub use tools::{register_tools_from_config, ShellToolConfig, ToolsConfig};
pub use traits::CallerIdentity;
pub use traits::*;
pub use transcript::{
    AgentReplyContext, InboundMessageRecord, OutboundReplyRecord, TranscriptSource,
};

// 画像生成（D-1076）。境界監査の行番号ベースラインをずらさないよう末尾に置く。
pub mod generate_image;
