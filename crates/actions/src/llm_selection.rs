use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::model_admin::{
    resolve_exact_model, ModelAdminError, ModelAdministration, ModelCatalog, ModelResolutionError,
    TurnModelProfile,
};
use crate::traits::{Action, ActionContext, ActionResult};

const SELECT_LLM_DESCRIPTION: &str = "この会話のモデルを変える（オーナーの /model と同じ）。\
     変更は次のターンから効き、会話が続くあいだ（また変えるまで）使われる。いま実行中のターンは今のモデルのまま終わる。\
     provider:model 形式、または一覧で一意なモデル名で指定。選べるモデルは get_system_info の selectable_models（/model と同じ一覧）。";

/// `select_llm` の説明とパラメータ。`available_providers` は llm_router の登録済み名
/// （`RuntimeInfo.available_providers`）。未登録プロバイダ名はここに載せない。
pub fn select_llm_schema(available_providers: &[String]) -> (String, serde_json::Value) {
    let mut providers: Vec<&str> = available_providers.iter().map(String::as_str).collect();
    providers.sort_unstable();
    providers.dedup();
    let providers_text = if providers.is_empty() {
        "（構成済みプロバイダなし）".to_string()
    } else {
        providers.join(", ")
    };
    let description = format!(
        "{SELECT_LLM_DESCRIPTION}構成済みプロバイダ: {providers_text}。\
         一覧で解決できない指定は何も変えず、選べる一覧を返す。"
    );
    let model_alias = json!({
        "type": "string",
        "description": format!(
            "この会話で使うモデル（provider:model または一覧で一意なモデル名）。構成済みプロバイダ: {providers_text}"
        )
    });
    let parameters = json!({
        "type": "object",
        "required": ["model_alias", "reason"],
        "properties": {
            "purpose": {
                "type": "string",
                "description": "LLMの用途（自由記述。例: 複雑な推論, 簡単な質問応答, 創作, コード生成, 要約）"
            },
            "model_alias": model_alias,
            "reason": {
                "type": "string",
                "description": "このモデルを選んだ理由（自由記述。過去の経験やメトリクスに基づく判断を書く）"
            }
        }
    });
    (description, parameters)
}

/// The model the running turn uses for all its LLM calls (D-1080: one model per turn).
pub fn current_turn_model(ctx: &ActionContext) -> String {
    let info = ctx.runtime_info.lock().unwrap().clone();
    info.active_model.unwrap_or(info.default_model)
}

/// D-1131: the models the agent can select: the gateway `/model` list
/// ([`ModelAdministration::list_models`]) with the providers whose models could not be read.
/// `Err` carries a message for the agent.
pub async fn selectable_models(
    models: Option<&dyn ModelAdministration>,
    agent_id: &str,
) -> Result<ModelCatalog, String> {
    let Some(models) = models else {
        return Err("モデル一覧を取得できない（この実行環境にはモデル一覧がない）".to_string());
    };
    models
        .list_models(agent_id)
        .await
        .map(|snapshot| ModelCatalog {
            models: snapshot.models,
            unavailable_providers: snapshot.unavailable_providers,
        })
        .map_err(|_| "モデル一覧を取得できない".to_string())
}

/// D-1135: the conversation session `/model` addresses for this turn. A subtask runs in its own
/// `subtask-*` session, which is not a conversation: `Err` carries a message for the agent.
pub fn conversation_session_id(ctx: &ActionContext) -> Result<&str, String> {
    const NOT_A_CONVERSATION: &str =
        "モデルの切り替えは会話の中でだけできる（このターンには /model で変えられる会話がない）";
    let Some(session_id) = ctx.session_id.as_deref() else {
        return Err(NOT_A_CONVERSATION.to_string());
    };
    let session = ctx
        .db
        .lock()
        .ok()
        .and_then(|conn| opencrab_db::queries::get_session(&conn, session_id).ok())
        .flatten();
    match session {
        Some(session) if session.mode != "subtask" => Ok(session_id),
        _ => Err(NOT_A_CONVERSATION.to_string()),
    }
}

/// D-1135: the model the conversation's next turn uses (`/model` list `current_model`).
pub async fn conversation_model(
    models: Option<&dyn ModelAdministration>,
    ctx: &ActionContext,
) -> Result<String, String> {
    let session_id = conversation_session_id(ctx)?;
    let Some(models) = models else {
        return Err(
            "この会話のモデルを取得できない（この実行環境にはモデル管理がない）".to_string(),
        );
    };
    models
        .list_session_models(&ctx.agent_id, session_id)
        .await
        .map(|list| list.snapshot.current_model)
        .map_err(|_| "この会話のモデルを取得できない".to_string())
}

/// LLM選択アクション — エージェントが自分でこの会話のモデルを変える
///
/// D-1135: the same change as the owner's `/model` command: the request is resolved against the
/// `/model` list (D-1131) and written with [`ModelAdministration::set_session_model`] (the
/// conversation's `Default` slot, `sessions.model`). The running turn keeps its model; the change
/// applies from the conversation's next turn and lasts until changed again.
#[derive(Default)]
pub struct SelectLlmAction {
    models: Option<Arc<dyn ModelAdministration>>,
}

impl SelectLlmAction {
    pub fn new(models: Option<Arc<dyn ModelAdministration>>) -> Self {
        Self { models }
    }
}

#[async_trait]
impl Action for SelectLlmAction {
    fn name(&self) -> &str {
        "select_llm"
    }

    fn description(&self) -> &str {
        // 静的文面。実行時の構成済み一覧は BridgedExecutor が select_llm_schema で上書きする。
        SELECT_LLM_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        select_llm_schema(&[]).1
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let purpose = args["purpose"].as_str().unwrap_or("conversation");
        let Some(requested) = args["model_alias"].as_str() else {
            return ActionResult::error("model_alias is required");
        };
        let reason = args["reason"].as_str().unwrap_or("");
        let not_changed = |why: &str| {
            ActionResult::error(&format!(
                "モデル '{requested}' に変えなかった: {why}。このターンのモデル: {}",
                current_turn_model(ctx)
            ))
        };

        let session_id = match conversation_session_id(ctx) {
            Ok(session_id) => session_id,
            Err(error) => return not_changed(&error),
        };
        let Some(models) = self.models.as_deref() else {
            return not_changed("この実行環境にはモデル管理がない");
        };
        let catalog = match selectable_models(Some(models), &ctx.agent_id).await {
            Ok(catalog) => catalog,
            Err(error) => return not_changed(&error),
        };
        let unresolved = |why: &str| {
            let mut message = format!("{why}。選べるモデル: [{}]", catalog.models.join(", "));
            if !catalog.unavailable_providers.is_empty() {
                let missing: Vec<String> = catalog
                    .unavailable_providers
                    .iter()
                    .map(|p| format!("{}（{}）", p.provider, p.error))
                    .collect();
                message.push_str(&format!(
                    "。モデル一覧を取得できなかったプロバイダ: [{}]",
                    missing.join(", ")
                ));
            }
            not_changed(&message)
        };
        let selected = match resolve_exact_model(&catalog.models, requested) {
            Ok(selected) => selected,
            Err(ModelResolutionError::InvalidArgs) => {
                return unresolved("指定が空か前後に空白がある")
            }
            Err(ModelResolutionError::NotFound) => return unresolved("選べるモデルの一覧にない"),
            Err(ModelResolutionError::Ambiguous) => {
                return unresolved(
                    "同じ名前のモデルが複数のプロバイダにある（provider:model で指定する）",
                )
            }
        };

        let conversation_model = match models
            .set_session_model(&ctx.agent_id, session_id, TurnModelProfile::Default, &selected)
            .await
        {
            Ok(snapshot) => snapshot.current_model,
            Err(ModelAdminError::Validation(message)) => return not_changed(&message),
            Err(ModelAdminError::TargetUnavailable) => {
                return not_changed("この会話は /model で変えられる会話ではない（参加者が自分だけの会話でだけ変えられる）")
            }
            Err(_) => return not_changed("この会話のモデルを変えられなかった"),
        };

        // Update the shared current_purpose so metrics are tagged correctly.
        if let Ok(mut current) = ctx.current_purpose.lock() {
            *current = purpose.to_string();
        }

        let current_turn = current_turn_model(ctx);
        ActionResult::success(json!({
            "changed": true,
            "conversation_model": conversation_model,
            "current_turn_model": current_turn,
            "note": format!(
                "この会話のモデルは {conversation_model} になった（/model と同じ）。次のターンから使われ、また変えるまで続く。いま実行中のターンは {current_turn} のまま終わる。"
            ),
            "purpose": purpose,
            "reason": reason,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::GetSystemInfoAction;
    use crate::model_admin::{EffortSnapshot, ModelSnapshot, SessionEffortList, SessionModelList};
    use crate::traits::CallerIdentity;

    const TURN_MODEL: &str = "chatgpt:gpt-6.1-terra";

    /// The `/model` administration of the tests: the `/model` list plus the session model slot
    /// (`sessions.model`) that `/model` and `select_llm` write.
    struct SessionModels {
        models: Vec<String>,
        unavailable: Vec<crate::model_admin::UnavailableProvider>,
        session_model: std::sync::Mutex<Option<String>>,
    }

    impl SessionModels {
        fn snapshot(&self, models: Vec<String>) -> ModelSnapshot {
            let configured_model = self.session_model.lock().unwrap().clone();
            ModelSnapshot {
                models,
                unavailable_providers: self.unavailable.clone(),
                current_model: configured_model.clone().unwrap_or(TURN_MODEL.to_string()),
                configured_model,
                default_model: TURN_MODEL.to_string(),
            }
        }
    }

    #[async_trait]
    impl ModelAdministration for SessionModels {
        async fn list_models(&self, _agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
            Ok(ModelSnapshot {
                models: self.models.clone(),
                unavailable_providers: self.unavailable.clone(),
                configured_model: None,
                current_model: TURN_MODEL.to_string(),
                default_model: TURN_MODEL.to_string(),
            })
        }
        async fn set_model(&self, _: &str, _: &str) -> Result<ModelSnapshot, ModelAdminError> {
            unreachable!()
        }
        async fn reset_model(&self, _: &str) -> Result<ModelSnapshot, ModelAdminError> {
            unreachable!()
        }
        async fn list_session_models(
            &self,
            agent_id: &str,
            session_id: &str,
        ) -> Result<SessionModelList, ModelAdminError> {
            assert_eq!((agent_id, session_id), ("agent-1", "session-1"));
            let snapshot = self.snapshot(self.models.clone());
            Ok(SessionModelList {
                realtime_current_model: snapshot.current_model.clone(),
                realtime_configured_model: None,
                snapshot,
            })
        }
        async fn set_session_model(
            &self,
            agent_id: &str,
            session_id: &str,
            profile: TurnModelProfile,
            model: &str,
        ) -> Result<ModelSnapshot, ModelAdminError> {
            assert_eq!((agent_id, session_id), ("agent-1", "session-1"));
            assert_eq!(profile, TurnModelProfile::Default);
            let canonical = resolve_exact_model(&self.models, model)?;
            *self.session_model.lock().unwrap() = Some(canonical);
            Ok(self.snapshot(self.models.clone()))
        }
        async fn reset_session_model(
            &self,
            _: &str,
            _: &str,
            _: TurnModelProfile,
        ) -> Result<ModelSnapshot, ModelAdminError> {
            unreachable!()
        }
        async fn list_session_reasoning_efforts(
            &self,
            _: &str,
            _: &str,
        ) -> Result<SessionEffortList, ModelAdminError> {
            unreachable!()
        }
        async fn set_session_reasoning_effort(
            &self,
            _: &str,
            _: &str,
            _: TurnModelProfile,
            _: &str,
        ) -> Result<EffortSnapshot, ModelAdminError> {
            unreachable!()
        }
        async fn reset_session_reasoning_effort(
            &self,
            _: &str,
            _: &str,
            _: TurnModelProfile,
        ) -> Result<EffortSnapshot, ModelAdminError> {
            unreachable!()
        }
    }

    fn session_models() -> Arc<SessionModels> {
        Arc::new(SessionModels {
            models: vec![
                "chatgpt:gpt-6.1-terra".to_string(),
                "chatgpt:gpt-6-sol".to_string(),
                "mock:test-model".to_string(),
            ],
            unavailable: Vec::new(),
            session_model: std::sync::Mutex::new(None),
        })
    }

    /// The `/model` list when the `ollama` provider could not list its models.
    fn session_models_without_ollama() -> Arc<SessionModels> {
        Arc::new(SessionModels {
            unavailable: vec![crate::model_admin::UnavailableProvider {
                provider: "ollama".to_string(),
                error: "failed to list Ollama models".to_string(),
            }],
            ..Arc::into_inner(session_models()).unwrap()
        })
    }

    fn insert_session(ctx: &ActionContext, id: &str, mode: &str) {
        let conn = ctx.db.lock().unwrap();
        opencrab_db::queries::insert_session(
            &conn,
            &opencrab_db::queries::SessionRow {
                id: id.to_string(),
                mode: mode.to_string(),
                theme: id.to_string(),
                phase: "active".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: json!(["agent-1"]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    /// A conversation turn of `agent-1` in `session_id`, running on [`TURN_MODEL`].
    fn test_context(session_id: &str) -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some(session_id.to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: Some(TURN_MODEL.to_string()),
                available_providers: vec!["chatgpt".to_string(), "mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        insert_session(&ctx, "session-1", "generic");
        insert_session(&ctx, "subtask-st-1", "subtask");
        (dir, ctx)
    }

    /// D-1135: select_llm writes this conversation's model like `/model` and says the change
    /// applies from the next turn; the running turn's model stays.
    #[tokio::test]
    async fn select_llm_changes_the_conversation_model_from_the_next_turn() {
        let (_dir, ctx) = test_context("session-1");
        let models = session_models();
        let result = SelectLlmAction::new(Some(models.clone()))
            .execute(
                &json!({"model_alias": "gpt-6-sol", "reason": "r", "purpose": "創作"}),
                &ctx,
            )
            .await;

        assert!(result.success, "{:?}", result.error);
        assert!(result.side_effects.is_empty());
        let data = result.data.unwrap();
        assert_eq!(data["changed"], true);
        assert_eq!(data["conversation_model"], "chatgpt:gpt-6-sol");
        assert_eq!(data["current_turn_model"], TURN_MODEL);
        let note = data["note"].as_str().unwrap();
        assert!(note.contains("次のターンから"), "{note}");
        assert!(data.get("duration").is_none(), "{data}");
        assert_eq!(
            models.session_model.lock().unwrap().as_deref(),
            Some("chatgpt:gpt-6-sol")
        );
        assert_eq!(current_turn_model(&ctx), TURN_MODEL);
        assert_eq!(*ctx.current_purpose.lock().unwrap(), "創作");
    }

    /// 未登録プロバイダは選択肢（説明文・パラメータ）に出ない。D-1135: 有効期間の引数は無い。
    #[test]
    fn select_llm_schema_lists_registered_providers_and_has_no_duration() {
        let (desc, params) = select_llm_schema(&["hermit".to_string(), "codex".to_string()]);
        let params_text = params.to_string();
        assert!(desc.contains("hermit"), "{desc}");
        assert!(desc.contains("codex"), "{desc}");
        assert!(
            !desc.contains("openai"),
            "未登録 openai が説明に出ている: {desc}"
        );
        assert!(params_text.contains("hermit"), "{params_text}");
        assert!(params_text.contains("codex"), "{params_text}");
        assert!(
            !params_text.contains("openai"),
            "未登録 openai がパラメータに出ている: {params_text}"
        );
        assert!(params["properties"].get("duration").is_none(), "{params}");
        assert!(desc.contains("この会話のモデル"), "{desc}");
        assert!(desc.contains("次のターンから"), "{desc}");
        assert!(!desc.contains("このターンの以降"), "{desc}");
    }

    /// D-1131: a name the `/model` list cannot resolve (production: "opus-5.5", or an
    /// unregistered provider) is rejected with the selectable list and changes nothing.
    #[tokio::test]
    async fn select_llm_rejects_unresolvable_name_with_choices_and_changes_nothing() {
        for requested in ["opus-5.5", "openai:codex"] {
            let (_dir, ctx) = test_context("session-1");
            let models = session_models();
            let result = SelectLlmAction::new(Some(models.clone()))
                .execute(
                    &json!({"model_alias": requested, "reason": "試したい"}),
                    &ctx,
                )
                .await;

            assert!(!result.success);
            let err = result.error.unwrap_or_default();
            assert!(err.contains(requested), "{err}");
            assert!(err.contains(TURN_MODEL), "このターンのモデルと一覧: {err}");
            assert!(err.contains("chatgpt:gpt-6-sol"), "選べる一覧: {err}");
            assert!(err.contains("mock:test-model"), "選べる一覧: {err}");
            assert_eq!(*models.session_model.lock().unwrap(), None);
            assert_eq!(*ctx.current_purpose.lock().unwrap(), "conversation");
        }
    }

    /// D-1135: a subtask runs in its own `subtask-*` session, which `/model` cannot address:
    /// select_llm there changes nothing and says switching is only available in a conversation.
    #[tokio::test]
    async fn select_llm_outside_a_conversation_changes_nothing() {
        for session_id in ["subtask-st-1", "missing"] {
            let (_dir, ctx) = test_context(session_id);
            let models = session_models();
            let result = SelectLlmAction::new(Some(models.clone()))
                .execute(&json!({"model_alias": "gpt-6-sol", "reason": "r"}), &ctx)
                .await;

            assert!(!result.success);
            let err = result.error.unwrap_or_default();
            assert!(err.contains("会話の中でだけ"), "{err}");
            assert_eq!(*models.session_model.lock().unwrap(), None);
        }
    }

    /// D-1135: get_system_info labels the model this turn runs on and this conversation's
    /// model (what the next turn uses); after select_llm only the conversation's model changes.
    #[tokio::test]
    async fn get_system_info_after_select_llm_shows_the_conversation_model() {
        let (_dir, ctx) = test_context("session-1");
        let models = session_models();
        let info = GetSystemInfoAction::new(Some(models.clone()));

        let before = info.execute(&json!({}), &ctx).await.data.unwrap();
        assert_eq!(before["current_turn_model"], TURN_MODEL);
        assert_eq!(before["conversation_model"], TURN_MODEL);

        let switched = SelectLlmAction::new(Some(models.clone()))
            .execute(
                &json!({"model_alias": "chatgpt:gpt-6-sol", "reason": "r"}),
                &ctx,
            )
            .await;
        assert!(switched.success, "{:?}", switched.error);

        let after = info.execute(&json!({}), &ctx).await.data.unwrap();
        assert_eq!(after["current_turn_model"], TURN_MODEL);
        assert_eq!(after["conversation_model"], "chatgpt:gpt-6-sol");
        assert!(after.get("active_model").is_none(), "{after}");
        assert_eq!(
            after["selectable_models"],
            json!([
                "chatgpt:gpt-6.1-terra",
                "chatgpt:gpt-6-sol",
                "mock:test-model"
            ])
        );
    }

    /// D-1141: a provider that cannot list its models does not block the others: select_llm
    /// still switches to a listed model, and an unresolved name names the missing provider.
    #[tokio::test]
    async fn select_llm_uses_the_partial_list_and_names_unavailable_providers() {
        let (_dir, ctx) = test_context("session-1");
        let models = session_models_without_ollama();

        let rejected = SelectLlmAction::new(Some(models.clone()))
            .execute(
                &json!({"model_alias": "ollama:llama3", "reason": "r"}),
                &ctx,
            )
            .await;
        assert!(!rejected.success);
        let err = rejected.error.unwrap_or_default();
        assert!(err.contains("chatgpt:gpt-6-sol"), "{err}");
        assert!(
            err.contains("ollama（failed to list Ollama models）"),
            "{err}"
        );

        let switched = SelectLlmAction::new(Some(models.clone()))
            .execute(&json!({"model_alias": "gpt-6-sol", "reason": "r"}), &ctx)
            .await;
        assert!(switched.success, "{:?}", switched.error);
        assert_eq!(
            models.session_model.lock().unwrap().as_deref(),
            Some("chatgpt:gpt-6-sol")
        );

        let info = GetSystemInfoAction::new(Some(models))
            .execute(&json!({}), &ctx)
            .await
            .data
            .unwrap();
        assert_eq!(info["selectable_models"].as_array().unwrap().len(), 3);
        assert_eq!(
            info["unavailable_providers"],
            json!([{"provider": "ollama", "error": "failed to list Ollama models"}])
        );
    }
}
