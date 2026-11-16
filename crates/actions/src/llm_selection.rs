use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::model_admin::{resolve_exact_model, ModelAdministration, ModelResolutionError};
use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

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
        "このターンの以降の LLM 呼び出しに使うモデルを切り替える。provider:model 形式、または一覧で\
         一意なモデル名で指定。選べるモデルは get_system_info の selectable_models（/model と同じ一覧）。\
         構成済みプロバイダ: {providers_text}。一覧で解決できない指定は切り替えず、選べる一覧を返す。"
    );
    let model_alias = json!({
        "type": "string",
        "description": format!(
            "使用するモデル（provider:model または一覧で一意なモデル名）。構成済みプロバイダ: {providers_text}"
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
            },
            "duration": {
                "type": "string",
                "description": "この設定の有効期間（自由記述。例: this_turn, this_session, until_task_complete）"
            }
        }
    });
    (description, parameters)
}

/// D-1131: the model the next LLM call of this turn uses: the `select_llm` override if set,
/// else the turn's model.
pub fn model_in_effect(ctx: &ActionContext) -> String {
    if let Some(model) = ctx.model_override.lock().ok().and_then(|m| m.clone()) {
        return model;
    }
    let info = ctx.runtime_info.lock().unwrap().clone();
    info.active_model.unwrap_or(info.default_model)
}

/// D-1131: the models the agent can select: the gateway `/model` list
/// ([`ModelAdministration::list_models`]). `Err` carries a message for the agent.
pub async fn selectable_models(
    models: Option<&dyn ModelAdministration>,
    agent_id: &str,
) -> Result<Vec<String>, String> {
    let Some(models) = models else {
        return Err("モデル一覧を取得できない（この実行環境にはモデル一覧がない）".to_string());
    };
    models
        .list_models(agent_id)
        .await
        .map(|snapshot| snapshot.models)
        .map_err(|_| "モデル一覧を取得できない（プロバイダのモデル一覧の取得に失敗）".to_string())
}

/// LLM選択アクション — エージェントが自ら使用モデルを切り替える
///
/// D-1131: the request is resolved with [`resolve_exact_model`] against the `/model` list, the
/// same resolution as the gateway `/model` command. Only a resolved canonical `provider:model` is
/// set as the turn's model override.
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
        "このターンの以降の LLM 呼び出しに使うモデルを切り替える。provider:model 形式、または一覧で一意なモデル名で指定。選べるモデルは get_system_info の selectable_models。"
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
        let duration = args["duration"].as_str().unwrap_or("this_turn");

        let available = match selectable_models(self.models.as_deref(), &ctx.agent_id).await {
            Ok(available) => available,
            Err(error) => {
                return ActionResult::error(&format!(
                    "モデル '{requested}' に切り替えなかった: {error}。現在のモデル: {}",
                    model_in_effect(ctx)
                ))
            }
        };
        let selected = match resolve_exact_model(&available, requested) {
            Ok(selected) => selected,
            Err(error) => {
                let why = match error {
                    ModelResolutionError::InvalidArgs => "指定が空か前後に空白がある",
                    ModelResolutionError::NotFound => "選べるモデルの一覧にない",
                    ModelResolutionError::Ambiguous => {
                        "同じ名前のモデルが複数のプロバイダにある（provider:model で指定する）"
                    }
                };
                return ActionResult::error(&format!(
                    "モデル '{requested}' に切り替えなかった: {why}。現在のモデル: {}。選べるモデル: [{}]",
                    model_in_effect(ctx),
                    available.join(", ")
                ));
            }
        };

        // Update the shared model_override so SkillEngine uses this model.
        if let Ok(mut current) = ctx.model_override.lock() {
            *current = Some(selected.clone());
        }

        // Update the shared current_purpose so metrics are tagged correctly.
        if let Ok(mut current) = ctx.current_purpose.lock() {
            *current = purpose.to_string();
        }

        ActionResult::success(json!({
            "switched": true,
            "selected": selected,
            "purpose": purpose,
            "reason": reason,
            "duration": duration,
        }))
        .with_side_effect(SideEffect::LlmSwitched {
            purpose: purpose.to_string(),
            model: selected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::GetSystemInfoAction;
    use crate::model_admin::{
        EffortSnapshot, ModelAdminError, ModelSnapshot, SessionEffortList, SessionModelList,
        TurnModelProfile,
    };
    use crate::traits::CallerIdentity;

    /// The `/model` list of the tests: what `ModelAdministration::list_models` returns.
    struct FixedModelList(Vec<String>);

    #[async_trait]
    impl ModelAdministration for FixedModelList {
        async fn list_models(&self, _agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
            Ok(ModelSnapshot {
                models: self.0.clone(),
                configured_model: None,
                current_model: "chatgpt:gpt-6.1-terra".to_string(),
                default_model: "chatgpt:gpt-6.1-terra".to_string(),
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
            _: &str,
            _: &str,
        ) -> Result<SessionModelList, ModelAdminError> {
            unreachable!()
        }
        async fn set_session_model(
            &self,
            _: &str,
            _: &str,
            _: TurnModelProfile,
            _: &str,
        ) -> Result<ModelSnapshot, ModelAdminError> {
            unreachable!()
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

    fn model_list() -> Arc<dyn ModelAdministration> {
        Arc::new(FixedModelList(vec![
            "chatgpt:gpt-6.1-terra".to_string(),
            "chatgpt:gpt-6-sol".to_string(),
            "mock:test-model".to_string(),
        ]))
    }

    fn select_llm() -> SelectLlmAction {
        SelectLlmAction::new(Some(model_list()))
    }

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            model_override: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: Some("chatgpt:gpt-6.1-terra".to_string()),
                available_providers: vec!["chatgpt".to_string(), "mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_select_llm_updates_model_override() {
        let (_dir, ctx) = test_context();
        let result = select_llm()
            .execute(
                &json!({
                    "model_alias": "mock:test-model",
                    "reason": "Cheaper for simple tasks",
                    "purpose": "conversation",
                }),
                &ctx,
            )
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.data.as_ref().unwrap()["switched"], true);
        assert_eq!(result.data.as_ref().unwrap()["selected"], "mock:test-model");
        assert_eq!(
            ctx.model_override.lock().unwrap().as_deref(),
            Some("mock:test-model")
        );
    }

    #[tokio::test]
    async fn test_select_llm_emits_side_effect() {
        let (_dir, ctx) = test_context();
        let result = select_llm()
            .execute(
                &json!({
                    "model_alias": "gpt-6-sol",
                    "reason": "Speed needed",
                }),
                &ctx,
            )
            .await;

        assert_eq!(result.side_effects.len(), 1);
        match &result.side_effects[0] {
            SideEffect::LlmSwitched { purpose, model } => {
                assert_eq!(model, "chatgpt:gpt-6-sol");
                assert_eq!(purpose, "conversation");
            }
            _ => panic!("Expected LlmSwitched side effect"),
        }
    }

    /// 未登録プロバイダは選択肢（説明文・パラメータ）に出ない。
    #[test]
    fn unregistered_provider_is_not_in_select_llm_choices() {
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
    }

    /// 未登録プロバイダの指定は一覧で解決できないので拒否する。
    #[tokio::test]
    async fn select_llm_rejects_unregistered_provider() {
        let (_dir, ctx) = test_context();
        let result = select_llm()
            .execute(
                &json!({
                    "model_alias": "openai:codex",
                    "reason": "QC で選ばれた未登録指定",
                }),
                &ctx,
            )
            .await;

        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("openai:codex"), "{err}");
        assert!(
            ctx.model_override.lock().unwrap().is_none(),
            "拒否したのに override を書いてはいけない"
        );
    }

    /// D-1131: a bare name the `/model` list cannot resolve (production: "opus-5.5", which the
    /// router used to hand to its default provider) is rejected with the selectable list instead
    /// of `switched:true`, and the turn's model stays.
    #[tokio::test]
    async fn select_llm_rejects_unresolvable_name_with_choices() {
        let (_dir, ctx) = test_context();
        let result = select_llm()
            .execute(
                &json!({"model_alias": "opus-5.5", "reason": "試したい"}),
                &ctx,
            )
            .await;

        assert!(!result.success);
        assert!(result.side_effects.is_empty());
        let err = result.error.unwrap_or_default();
        assert!(err.contains("opus-5.5"), "{err}");
        assert!(
            err.contains("chatgpt:gpt-6.1-terra"),
            "現在のモデルと一覧: {err}"
        );
        assert!(err.contains("chatgpt:gpt-6-sol"), "選べる一覧: {err}");
        assert!(err.contains("mock:test-model"), "選べる一覧: {err}");
        assert!(ctx.model_override.lock().unwrap().is_none());
    }

    /// D-1131: a unique bare model name resolves to its canonical provider:model, as `/model`.
    #[tokio::test]
    async fn select_llm_returns_canonical_model_for_bare_name() {
        let (_dir, ctx) = test_context();
        let result = select_llm()
            .execute(&json!({"model_alias": "gpt-6-sol", "reason": "r"}), &ctx)
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            result.data.as_ref().unwrap()["selected"],
            "chatgpt:gpt-6-sol"
        );
        assert_eq!(
            ctx.model_override.lock().unwrap().as_deref(),
            Some("chatgpt:gpt-6-sol")
        );
    }

    /// D-1131: get_system_info after select_llm shows the model now in effect and the choices.
    #[tokio::test]
    async fn get_system_info_after_select_llm_shows_new_model_and_choices() {
        let (_dir, ctx) = test_context();
        let info = GetSystemInfoAction::new(Some(model_list()));

        let before = info.execute(&json!({}), &ctx).await;
        assert_eq!(
            before.data.as_ref().unwrap()["active_model"],
            "chatgpt:gpt-6.1-terra"
        );

        let switched = select_llm()
            .execute(
                &json!({"model_alias": "chatgpt:gpt-6-sol", "reason": "r"}),
                &ctx,
            )
            .await;
        assert!(switched.success, "{:?}", switched.error);

        let after = info.execute(&json!({}), &ctx).await;
        let data = after.data.unwrap();
        assert_eq!(data["active_model"], "chatgpt:gpt-6-sol");
        assert_eq!(
            data["selectable_models"],
            json!([
                "chatgpt:gpt-6.1-terra",
                "chatgpt:gpt-6-sol",
                "mock:test-model"
            ])
        );
    }
}
