pub mod a2ui;
pub mod agent_gateway;
pub mod agent_runtime;
pub mod bridge;
pub mod common;
pub mod dispatcher;
pub mod learning;
pub mod llm_analysis;
pub mod llm_evaluation;
pub mod llm_selection;
pub mod memory_access;
pub mod memory_units;
pub mod search;
pub mod session_runtime;
pub mod skill_management;
pub mod soul;
pub mod subtask;
pub mod subtask_notify;
pub mod subtask_registries;
pub mod task_ledger;
pub mod timed_fire;
pub mod tools;
pub mod traits;
pub mod transcript;
pub mod webhook_target;
pub mod workspace;

pub mod run_request;

pub use a2ui::{send_ui, send_ui_definition};
pub use agent_gateway::{
    is_start_declined, kinds as gateway_kinds, AgentGatewayLifecycle, AgentGatewayRegistry,
    GatewayIdentityProvisioning, GatewayKeyProvisioning, GatewayNostrPassthrough, ProvisionedKey,
    SharedAgentGateway, StartDeclined,
};
pub use agent_runtime::AgentRuntime;
pub use bridge::{
    tool_policy, BridgedExecutor, SubEngineGatewayActions, ToolEvent, ToolEventSink,
    ToolEventStatus, ToolPolicy, CORE_DISPATCHABLE_ACTIONS, CORE_INLINE_ACTIONS, MCP_TOOL_PREFIX,
    OWNER_ONLY_ACTIONS, REJECTION_CODE_PREFIX, TRUSTED_ONLY_ACTIONS,
};
pub use dispatcher::ActionDispatcher;
pub use run_request::{LiveInboundScope, RunRequest};
pub use session_runtime::{SessionLocks, SessionRuntime};
pub use subtask::{
    cancel_subtask, default_non_dispatch_tools, steer_subtask, CancelOutcome, NoopCompletionSink,
    SettleKind, SharedExecutor, SpawnedSubtask, SteerOutcome, SubtaskCompletionSink,
    SubtaskLifecycle, SubtaskRegistry, SubtaskSettled, SubtaskToolDispatcher,
    DEFAULT_DISPATCH_TIMEOUT_SECS, STEER_LOG_TYPE,
};
pub use subtask_notify::{
    NoopLifecycleNotifier, NoopRunNotifier, NotifyTarget, NotifyTargetError,
    SubtaskLifecycleNotifier, SubtaskNotifiers, SubtaskNotifySession, SubtaskRunInfo,
    SubtaskRunNotifier,
};
pub use subtask_registries::SubtaskRegistries;
pub use timed_fire::{
    new_turn_id, prompt_preview, FireTarget, TimedFireRequest, TimedFireRouter,
    TimedFireSelfCheckIssue, TimedFireSink, TransportFire, TransportFireEnv,
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
    AgentReplyContext, InboundMessageRecord, InteractionRecord, OutboundReplyRecord,
    TranscriptSource,
};
pub use webhook_target::{
    attachment_filename, build_message_with_optional_attachment, build_part_messages,
    build_webhook_body, chunk_text, has_activity_default, record_webhook_delivery_failure,
    redact_secrets, redact_webhook_url, resolve_activity_webhook, resolve_subtask_webhook,
    validate_webhook_url, WebhookAttachment, WebhookConfig, WebhookMessage, WebhookResolution,
    WebhookSource, ATTACHMENT_CONTENT_TYPE, ATTACHMENT_MAX_BYTES, ATTACHMENT_PREVIEW_CHARS,
    ATTACHMENT_THRESHOLD_CHARS,
};
