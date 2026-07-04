use indexmap::IndexMap;
use std::sync::Arc;

use crate::common::*;
use crate::learning::*;
use crate::llm_analysis::*;
use crate::llm_evaluation::*;
use crate::llm_selection::*;
use crate::memory_access::*;
use crate::search::*;
use crate::skill_management::*;
use crate::soul::*;
use crate::task_ledger::*;
use crate::traits::*;
use crate::workspace::*;

/// アクションディスパッチャー
pub struct ActionDispatcher {
    actions: IndexMap<String, Arc<dyn Action>>,
}

impl ActionDispatcher {
    pub fn new() -> Self {
        let mut dispatcher = Self {
            actions: IndexMap::new(),
        };

        // 共通アクション登録
        dispatcher.register(Arc::new(GenerateInnerVoiceAction));
        dispatcher.register(Arc::new(UpdateImpressionAction));
        dispatcher.register(Arc::new(DeclareDoneAction));
        dispatcher.register(Arc::new(GetSystemInfoAction));

        // ワークスペースアクション登録
        dispatcher.register(Arc::new(WsReadAction));
        dispatcher.register(Arc::new(WsWriteAction));
        dispatcher.register(Arc::new(WsEditAction));
        dispatcher.register(Arc::new(WsListAction));
        dispatcher.register(Arc::new(WsDeleteAction));
        dispatcher.register(Arc::new(WsMkdirAction));

        // 学習アクション登録
        dispatcher.register(Arc::new(LearnFromExperienceAction));
        dispatcher.register(Arc::new(LearnFromPeerAction));
        dispatcher.register(Arc::new(ReflectAndLearnAction));

        // 検索アクション登録
        dispatcher.register(Arc::new(SearchMyHistoryAction));
        dispatcher.register(Arc::new(SummarizeAndSaveAction));
        dispatcher.register(Arc::new(CreateMySkillAction));
        dispatcher.register(Arc::new(BrowseMemoryIndexAction));
        dispatcher.register(Arc::new(RetrieveMemoryNodesAction));
        dispatcher.register(Arc::new(SearchMemoryIndexAction));

        // LLM関連アクション登録
        dispatcher.register(Arc::new(SelectLlmAction));
        dispatcher.register(Arc::new(EvaluateResponseAction));
        dispatcher.register(Arc::new(AnalyzeLlmUsageAction));
        dispatcher.register(Arc::new(RecallModelExperiencesAction));
        dispatcher.register(Arc::new(SaveModelInsightAction));

        // Soul関連アクション登録
        dispatcher.register(Arc::new(UpdateInstructionsAction));

        // タスク台帳アクション登録
        dispatcher.register(Arc::new(OpenTaskAction));
        dispatcher.register(Arc::new(UpdateTaskContractAction));
        dispatcher.register(Arc::new(RecordTaskProgressAction));
        dispatcher.register(Arc::new(CloseTaskAction));
        dispatcher.register(Arc::new(GetTaskAction));

        dispatcher
    }

    pub fn register(&mut self, action: Arc<dyn Action>) {
        self.actions.insert(action.name().to_string(), action);
    }

    /// 指定名のアクションが登録されているか。
    ///
    /// bridge の gateway フォールバック判定に使う（"Unknown action: {name}" という
    /// エラー文言の文字列比較で判定すると、実アクションが同文を返した場合に
    /// 誤ルートするため）。
    pub fn has_action(&self, name: &str) -> bool {
        self.actions.contains_key(name)
    }

    /// アクションを実行
    pub async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &ActionContext,
    ) -> ActionResult {
        match self.actions.get(name) {
            Some(action) => action.execute(args, ctx).await,
            None => ActionResult::error(&format!("Unknown action: {name}")),
        }
    }

    /// 利用可能なアクション定義を取得
    pub fn get_definitions(&self, filter: &[String]) -> Vec<ActionDefinition> {
        self.actions
            .values()
            .filter(|a| filter.is_empty() || filter.contains(&a.name().to_string()))
            .map(|a| ActionDefinition {
                name: a.name().to_string(),
                description: a.description().to_string(),
                parameters: a.parameters(),
            })
            .collect()
    }

    /// 登録済みアクション名の一覧を取得
    pub fn action_names(&self) -> Vec<String> {
        self.actions.keys().cloned().collect()
    }
}

impl Default for ActionDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CallerIdentity;
    use serde_json::json;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            caller: CallerIdentity::Owner,
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
        };
        (dir, ctx)
    }

    #[test]
    fn test_all_actions_registered() {
        let dispatcher = ActionDispatcher::new();
        let names = dispatcher.action_names();
        assert!(
            names.len() >= 20,
            "Expected at least 20 actions, got {}",
            names.len()
        );
        assert!(names.contains(&"ws_read".to_string()));
        assert!(names.contains(&"ws_write".to_string()));
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let dispatcher = ActionDispatcher::new();
        let (_dir, ctx) = test_context();
        let result = dispatcher.execute("nonexistent", &json!({}), &ctx).await;
        assert!(!result.success);
    }

    #[test]
    fn test_get_definitions_all() {
        let dispatcher = ActionDispatcher::new();
        let defs = dispatcher.get_definitions(&[]);
        assert_eq!(defs.len(), dispatcher.action_names().len());
    }

    #[test]
    fn test_get_definitions_filtered() {
        let dispatcher = ActionDispatcher::new();
        let defs = dispatcher.get_definitions(&["generate_inner_voice".to_string()]);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "generate_inner_voice");
    }
}
