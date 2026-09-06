use super::super::super::*;
use opencrab_gateway::GatewayCaller;

/// 受け取った settle を記録する `SubtaskCompletionSink`。
#[derive(Default)]
pub(crate) struct RecordingSink {
    settled: std::sync::Mutex<Vec<SubtaskSettled>>,
}

impl RecordingSink {
    pub(crate) fn settled(&self) -> Vec<SubtaskSettled> {
        self.settled.lock().unwrap().clone()
    }
}

impl SubtaskCompletionSink for RecordingSink {
    fn session_prefix(&self) -> &'static str {
        ""
    }
    fn forwards_progress(&self) -> bool {
        true
    }
    fn deliver_continuation(&self, ev: SubtaskSettled) {
        self.settled.lock().unwrap().push(ev);
    }
}

/// 走行中扱いの subtask を 1 件だけ持つ registry。
pub(crate) fn registry_with(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
) -> SubtaskRegistry {
    registry_with_caller(
        subtask_id,
        session_id,
        parent_session_id,
        opencrab_actions::CallerIdentity::Agent,
    )
}

/// 親ターンの呼び出し元を指定して 1 件登録した registry（#298）。
pub(crate) fn registry_with_caller(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
    caller: opencrab_actions::CallerIdentity,
) -> SubtaskRegistry {
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: "job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    registry
}

/// steer テスト用: `steerable=true` の subtask を 1 件登録した registry（#647）。
pub(crate) fn registry_with_steerable(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
    caller: opencrab_actions::CallerIdentity,
) -> SubtaskRegistry {
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: "job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: true,
        },
    );
    registry
}

pub(crate) fn sub_ctx(session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
        .with_session_id(session_id)
        .with_depth(1)
}
