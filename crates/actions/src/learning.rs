use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

/// 経験からの学習アクション
pub struct LearnFromExperienceAction;

#[async_trait]
impl Action for LearnFromExperienceAction {
    fn name(&self) -> &str {
        "learn_from_experience"
    }

    fn description(&self) -> &str {
        "成功/失敗した経験から新しいスキルを抽出して学習する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["experience", "outcome", "lesson"],
            "properties": {
                "experience": {
                    "type": "string",
                    "description": "どのような経験だったか"
                },
                "outcome": {
                    "type": "string",
                    "enum": ["success", "failure", "partial"],
                    "description": "結果（成功/失敗/部分的成功）"
                },
                "lesson": {
                    "type": "string",
                    "description": "この経験から学んだこと"
                },
                "skill_name": {
                    "type": "string",
                    "description": "抽出するスキルの名前"
                },
                "situation_pattern": {
                    "type": "string",
                    "description": "このスキルが適用できる状況パターン"
                },
                "guidance": {
                    "type": "string",
                    "description": "具体的な行動指針"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let skill_name = args["skill_name"]
            .as_str()
            .unwrap_or("unnamed")
            .to_string();
        let skill_id = uuid::Uuid::new_v4().to_string();

        let skill = opencrab_db::queries::SkillRow {
            id: skill_id.clone(),
            agent_id: ctx.agent_id.clone(),
            name: skill_name.clone(),
            description: args["lesson"].as_str().unwrap_or("").to_string(),
            situation_pattern: args["situation_pattern"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            guidance: args["guidance"].as_str().unwrap_or("").to_string(),
            source_type: "experience".to_string(),
            source_context: args["experience"].as_str().map(|s| s.to_string()),
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
        };

        if let Ok(conn) = ctx.db.lock() {
            if let Err(e) = opencrab_db::queries::insert_skill(&conn, &skill) {
                return ActionResult::error(&format!("Failed to save skill: {e}"));
            }
        }

        ActionResult::success(json!({
            "skill_id": skill_id,
            "skill_name": skill_name,
            "message": "新しいスキルを獲得しました",
        }))
        .with_side_effect(SideEffect::SkillAcquired { skill_id })
    }
}

/// 他者からの学習アクション
pub struct LearnFromPeerAction;

#[async_trait]
impl Action for LearnFromPeerAction {
    fn name(&self) -> &str {
        "learn_from_peer"
    }

    fn description(&self) -> &str {
        "他のエージェントの効果的なパターンを観察して学習する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["peer_name", "observed_pattern", "lesson"],
            "properties": {
                "peer_name": {
                    "type": "string",
                    "description": "観察した相手の名前"
                },
                "observed_pattern": {
                    "type": "string",
                    "description": "観察した効果的なパターン"
                },
                "lesson": {
                    "type": "string",
                    "description": "学んだこと"
                },
                "skill_name": {
                    "type": "string",
                    "description": "抽出するスキルの名前"
                },
                "guidance": {
                    "type": "string",
                    "description": "自分に適用する際の行動指針"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let skill_name = args["skill_name"]
            .as_str()
            .unwrap_or("unnamed")
            .to_string();
        let skill_id = uuid::Uuid::new_v4().to_string();

        let source_context = format!(
            "Learned from {}: {}",
            args["peer_name"].as_str().unwrap_or("unknown"),
            args["observed_pattern"].as_str().unwrap_or("")
        );

        let skill = opencrab_db::queries::SkillRow {
            id: skill_id.clone(),
            agent_id: ctx.agent_id.clone(),
            name: skill_name.clone(),
            description: args["lesson"].as_str().unwrap_or("").to_string(),
            situation_pattern: String::new(),
            guidance: args["guidance"].as_str().unwrap_or("").to_string(),
            source_type: "peer".to_string(),
            source_context: Some(source_context),
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
        };

        if let Ok(conn) = ctx.db.lock() {
            if let Err(e) = opencrab_db::queries::insert_skill(&conn, &skill) {
                return ActionResult::error(&format!("Failed to save skill: {e}"));
            }
        }

        ActionResult::success(json!({
            "skill_id": skill_id,
            "skill_name": skill_name,
            "message": "他者から新しいスキルを学びました",
        }))
        .with_side_effect(SideEffect::SkillAcquired { skill_id })
    }
}

/// 振り返り学習アクション
pub struct ReflectAndLearnAction;

#[async_trait]
impl Action for ReflectAndLearnAction {
    fn name(&self) -> &str {
        "reflect_and_learn"
    }

    fn description(&self) -> &str {
        "過去のやりとりを振り返って学びを抽出する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "reflection": {
                    "type": "string",
                    "description": "振り返りの内容"
                },
                "insights": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "得られた洞察のリスト"
                },
                "action_items": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "今後のアクションアイテム"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let reflection = args["reflection"].as_str().unwrap_or("");

        // キュレーション記憶として保存
        let memory = opencrab_db::queries::CuratedMemoryRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: ctx.agent_id.clone(),
            category: "reflection".to_string(),
            content: format!(
                "振り返り: {}\n洞察: {:?}\nアクション: {:?}",
                reflection,
                args["insights"],
                args["action_items"]
            ),
        };

        if let Ok(conn) = ctx.db.lock() {
            let _ = opencrab_db::queries::upsert_curated_memory(&conn, &memory);
        }

        ActionResult::success(json!({
            "reflected": true,
            "reflection": reflection,
        }))
    }
}
