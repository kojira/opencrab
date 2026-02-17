use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

/// LLM選択アクション
pub struct SelectLlmAction;

#[async_trait]
impl Action for SelectLlmAction {
    fn name(&self) -> &str {
        "select_llm"
    }

    fn description(&self) -> &str {
        "タスクに応じて使用するLLMを選択する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["purpose", "model_alias", "reason"],
            "properties": {
                "purpose": {
                    "type": "string",
                    "description": "LLMの用途",
                    "enum": ["thinking", "conversation", "analysis", "tool_calling", "creative"]
                },
                "model_alias": {
                    "type": "string",
                    "description": "使用するモデルのエイリアス（fast, smart, reasoning, local等）"
                },
                "reason": {
                    "type": "string",
                    "description": "このモデルを選んだ理由"
                },
                "duration": {
                    "type": "string",
                    "description": "この設定の有効期間",
                    "enum": ["this_turn", "this_session", "permanent"]
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ActionContext) -> ActionResult {
        let purpose = args["purpose"].as_str().unwrap_or("conversation");
        let model_alias = args["model_alias"].as_str().unwrap_or("smart");
        let reason = args["reason"].as_str().unwrap_or("");
        let duration = args["duration"].as_str().unwrap_or("this_turn");

        // Note: 実際のLLM切り替えはエンジン側で処理する
        // ここでは結果を返すだけ

        ActionResult::success(json!({
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
