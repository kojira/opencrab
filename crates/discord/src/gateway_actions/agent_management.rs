//! エージェント管理操作 (create_skill)
//!
//! `update_memory_index_config` / `add_allowed_command` / `list_allowed_commands` /
//! `remove_allowed_command` は #157 S1 で gateway 非依存層（`opencrab_server` の
//! `crate::agent_management`）へ移設済み。いずれも serenity を参照せず DB と実行許可
//! 設定だけに依存していたため、Discord 経由のターンにしか出ないのが不具合だった。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use super::DiscordGatewayActions;

impl DiscordGatewayActions {
    pub(crate) fn execute_create_skill(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // owner / co_agent / trusted_user の許可リスト（将来 variant が増えても fail-closed）。
        if !matches!(
            ctx.caller,
            GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
        ) {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはtrusted userのみ実行できます".to_string()),
            };
        }
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("name is required".to_string()),
                }
            }
        };
        let description = match args.get("description").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("description is required".to_string()),
                }
            }
        };
        let guidance = args.get("guidance").and_then(|v| v.as_str()).unwrap_or("");

        let conn = self.db.lock().unwrap();

        // Deduplication: check if skill with same name exists (non-archived)
        if let Ok(Some(existing)) =
            opencrab_db::queries::find_skill_by_name(&conn, &ctx.agent_id, name)
        {
            let mut updated = existing;
            updated.description = description.to_string();
            updated.guidance = guidance.to_string();
            if let Err(e) = opencrab_db::queries::update_skill(&conn, &updated) {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Failed to update existing skill: {e}")),
                };
            }
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "id": updated.id,
                    "name": name,
                    "action": "updated"
                })),
                error: None,
            };
        }

        // Check archived skills
        if let Ok(Some(existing)) =
            opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name)
        {
            let mut updated = existing;
            updated.archived = false;
            updated.is_active = true;
            updated.description = description.to_string();
            updated.guidance = guidance.to_string();
            if let Err(e) = opencrab_db::queries::update_skill(&conn, &updated) {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Failed to restore archived skill: {e}")),
                };
            }
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "id": updated.id,
                    "name": name,
                    "action": "restored"
                })),
                error: None,
            };
        }

        let id = uuid::Uuid::new_v4().to_string();
        let row = opencrab_db::queries::SkillRow {
            id: id.clone(),
            agent_id: ctx.agent_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            situation_pattern: String::new(),
            guidance: guidance.to_string(),
            source_type: "acquired".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "\"agent\"".to_string(),
            archived: false,
        };

        if let Err(e) = opencrab_db::queries::insert_skill(&conn, &row) {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Failed to create skill: {e}")),
            };
        }

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "id": id,
                "name": name,
                "action": "created"
            })),
            error: None,
        }
    }
}
