use async_trait::async_trait;
use serde_json::json;

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
        "タスクに応じて使用するLLMモデルを切り替える。provider:model 形式またはエイリアスで指定。\
         構成済みプロバイダ: {providers_text}。未登録のプロバイダは拒否される。"
    );
    let model_alias = json!({
        "type": "string",
        "description": format!(
            "使用するモデル（provider:model またはエイリアス）。構成済みプロバイダ: {providers_text}"
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

/// `provider:model` のプロバイダ部が構成済みか。エイリアス（コロン無し）はここでは
/// 通す（解決は router）。未登録プロバイダは実行前に拒否する。
pub fn reject_unregistered_model_spec(
    spec: &str,
    available_providers: &[String],
) -> Result<(), String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("model_alias is empty".to_string());
    }
    let Some((provider, model)) = spec.split_once(':') else {
        return Ok(());
    };
    if provider.is_empty() || model.is_empty() {
        return Err(format!("invalid provider:model spec: {spec}"));
    }
    if available_providers.iter().any(|p| p == provider) {
        return Ok(());
    }
    let mut registered: Vec<&str> = available_providers.iter().map(String::as_str).collect();
    registered.sort_unstable();
    Err(format!(
        "未登録の LLM プロバイダ '{provider}' は使えません（指定: {spec}）。構成済み: [{}]",
        registered.join(", ")
    ))
}

/// LLM選択アクション — エージェントが自ら使用モデルを切り替える
pub struct SelectLlmAction;

#[async_trait]
impl Action for SelectLlmAction {
    fn name(&self) -> &str {
        "select_llm"
    }

    fn description(&self) -> &str {
        // 静的文面。実行時の構成済み一覧は BridgedExecutor が select_llm_schema で上書きする。
        "タスクに応じて使用するLLMモデルを切り替える。provider:model 形式またはエイリアスで指定。未登録のプロバイダは拒否される。"
    }

    fn parameters(&self) -> serde_json::Value {
        select_llm_schema(&[]).1
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let purpose = args["purpose"].as_str().unwrap_or("conversation");
        let model_alias = args["model_alias"].as_str().unwrap_or("smart");
        let reason = args["reason"].as_str().unwrap_or("");
        let duration = args["duration"].as_str().unwrap_or("this_turn");

        let available_providers = ctx
            .runtime_info
            .lock()
            .map(|info| info.available_providers.clone())
            .unwrap_or_default();
        if let Err(e) = reject_unregistered_model_spec(model_alias, &available_providers) {
            return ActionResult::error(&e);
        }

        // Update the shared model_override so SkillEngine uses this model.
        if let Ok(mut current) = ctx.model_override.lock() {
            *current = Some(model_alias.to_string());
        }

        // Update the shared current_purpose so metrics are tagged correctly.
        if let Ok(mut current) = ctx.current_purpose.lock() {
            *current = purpose.to_string();
        }

        ActionResult::success(json!({
            "switched": true,
            "selected": model_alias,
            "purpose": purpose,
            "reason": reason,
            "duration": duration,
        }))
        .with_side_effect(SideEffect::LlmSwitched {
            purpose: purpose.to_string(),
            model: model_alias.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CallerIdentity;
    use std::sync::Arc;

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
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_select_llm_updates_model_override() {
        let (_dir, ctx) = test_context();
        let action = SelectLlmAction;

        let result = action
            .execute(
                &json!({
                    "model_alias": "mock:test-model",
                    "reason": "Cheaper for simple tasks",
                    "purpose": "conversation",
                }),
                &ctx,
            )
            .await;

        assert!(result.success);
        assert_eq!(result.data.as_ref().unwrap()["switched"], true);
        assert_eq!(result.data.as_ref().unwrap()["selected"], "mock:test-model");

        // Verify model_override was updated.
        let override_val = ctx.model_override.lock().unwrap();
        assert_eq!(override_val.as_deref(), Some("mock:test-model"));
    }

    #[tokio::test]
    async fn test_select_llm_emits_side_effect() {
        let (_dir, ctx) = test_context();
        let action = SelectLlmAction;

        let result = action
            .execute(
                &json!({
                    "model_alias": "fast",
                    "reason": "Speed needed",
                }),
                &ctx,
            )
            .await;

        assert_eq!(result.side_effects.len(), 1);
        match &result.side_effects[0] {
            SideEffect::LlmSwitched { purpose, model } => {
                assert_eq!(model, "fast");
                assert_eq!(purpose, "conversation");
            }
            _ => panic!("Expected LlmSwitched side effect"),
        }
    }

    /// (a) 未登録プロバイダは選択肢（説明文・パラメータ）に出ない。
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

    /// (b) 未登録プロバイダを強制指定したら選択時に拒否する。
    #[tokio::test]
    async fn select_llm_rejects_unregistered_provider() {
        let (_dir, ctx) = test_context();
        let result = SelectLlmAction
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
        assert!(err.contains("openai"), "{err}");
        assert!(err.contains("openai:codex"), "{err}");
        assert!(
            ctx.model_override.lock().unwrap().is_none(),
            "拒否したのに override を書いてはいけない"
        );
    }

    /// (c) 登録済みプロバイダは従来どおり通る。
    #[tokio::test]
    async fn select_llm_accepts_registered_provider() {
        let (_dir, ctx) = test_context();
        let result = SelectLlmAction
            .execute(
                &json!({
                    "model_alias": "mock:test-model",
                    "reason": "構成済み",
                }),
                &ctx,
            )
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            ctx.model_override.lock().unwrap().as_deref(),
            Some("mock:test-model")
        );
    }
}
