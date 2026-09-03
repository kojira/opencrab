mod event;
mod executor;
mod policy;
mod rejection;
mod subengine;

pub use event::{ToolEvent, ToolEventSink, ToolEventStatus};
pub use executor::BridgedExecutor;
pub use policy::{
    tool_policy, ToolPolicy, CORE_DISPATCHABLE_ACTIONS, CORE_INLINE_ACTIONS, MCP_TOOL_PREFIX,
    OWNER_ONLY_ACTIONS, TRUSTED_ONLY_ACTIONS,
};
pub use rejection::REJECTION_CODE_PREFIX;
pub use subengine::SubEngineGatewayActions;

use policy::MAX_DEPTH;
pub(crate) use rejection::{gateway_reject, is_rejection};

use opencrab_core::FunctionDefinition;

/// [`BridgedExecutor`] の実効ツールがどの production slot から来たか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolSlot {
    Dispatcher,
    Gateway,
    Mcp,
}

/// `list_tools` と同じ gate を通った定義と、dispatch に使う class 索引の組。
pub struct EffectiveToolDefinition {
    pub definition: FunctionDefinition,
    pub class: Option<opencrab_gateway::ToolClass>,
    pub slot: ToolSlot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorRuntimeState {
    pub model_override: Option<String>,
    pub current_purpose: String,
}
