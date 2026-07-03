//! エージェント管理操作 (memory_index, allowed_commands, create_skill)

use serde_json::json;
use tracing::error;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use super::DiscordGatewayActions;

impl DiscordGatewayActions {
    pub(crate) fn execute_update_memory_index_config(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let batch_size = args.get("batch_size").and_then(|v| v.as_i64());
        let threshold = args.get("threshold").and_then(|v| v.as_i64());

        if batch_size.is_none() && threshold.is_none() {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("batch_sizeまたはthresholdの少なくとも1つが必要です".to_string()),
            };
        }

        let conn = self.db.lock().unwrap();

        let current = opencrab_db::queries::get_memory_index_config(&conn, &ctx.agent_id);
        let (current_batch_size, current_threshold) = match &current {
            Ok(cfg) => (cfg.batch_size, cfg.threshold),
            Err(_) => (
                opencrab_db::queries::BATCH_SIZE_DEFAULT,
                opencrab_db::queries::THRESHOLD_DEFAULT,
            ),
        };

        let new_batch_size = batch_size.unwrap_or(current_batch_size);
        let new_threshold = threshold.unwrap_or(current_threshold);

        match opencrab_db::queries::upsert_memory_index_config(
            &conn,
            &ctx.agent_id,
            new_batch_size,
            new_threshold,
        ) {
            Ok(updated) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "agent_id": ctx.agent_id,
                    "previous": {
                        "batch_size": current_batch_size,
                        "threshold": current_threshold,
                    },
                    "current": {
                        "batch_size": updated.batch_size,
                        "threshold": updated.threshold,
                    },
                })),
                error: None,
            },
            Err(e) => {
                error!("upsert_memory_index_config failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("メモリインデックス設定の更新に失敗: {e}")),
                }
            }
        }
    }

    pub(crate) async fn execute_rebuild_memory_index(
        &self,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let llm_client = match &self.llm_client {
            Some(client) => client.clone(),
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("LLMクライアントが設定されていません".to_string()),
                }
            }
        };

        let config = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::get_memory_index_config(&conn, &ctx.agent_id).unwrap_or_else(
                |_| opencrab_db::queries::AgentMemoryIndexConfig {
                    agent_id: ctx.agent_id.clone(),
                    batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                    threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                    updated_at: String::new(),
                },
            )
        };

        let (persona_name, personality) = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                .ok()
                .flatten()
                .map(|a| (a.persona_name, a.personality))
                .unwrap_or_default()
        };

        match opencrab_core::memory_index::IndexBuilder::rebuild_index(
            &self.db,
            &ctx.agent_id,
            llm_client.as_ref(),
            &self.default_model,
            config.batch_size as usize,
            &persona_name,
            personality.as_deref(),
        )
        .await
        {
            Ok(result) => GatewayActionResult {
                success: true,
                data: Some(serde_json::json!({
                    "agent_id": ctx.agent_id,
                    "logs_indexed": result.logs_indexed,
                    "nodes_created": result.nodes_created,
                    "message": format!(
                        "メモリインデックスを再構築しました（{}件のログ → {}ノード作成）",
                        result.logs_indexed,
                        result.nodes_created,
                    ),
                })),
                error: None,
            },
            Err(e) => {
                error!("rebuild_memory_index failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("メモリインデックスの再構築に失敗: {e}")),
                }
            }
        }
    }

    pub(crate) fn execute_add_allowed_command(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        if ctx.caller != GatewayCaller::Owner {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはオーナーのみ実行できます".to_string()),
            };
        }

        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("commandパラメータが必要です".to_string()),
                }
            }
        };

        if !command
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "コマンド名に無効な文字が含まれています: {}（英数字・ハイフン・アンダースコアのみ使用可）",
                    command
                )),
            };
        }

        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::add_agent_allowed_command(
            &conn,
            &ctx.agent_id,
            command,
            "owner",
        ) {
            Ok(true) => {
                // Update in-memory tools_config
                drop(conn);
                if let Ok(mut cfg) = self.tools_config.write() {
                    if let Some(ref mut shell) = cfg.shell {
                        let cmd_str = command.to_string();
                        if !shell.allowed_commands.contains(&cmd_str) {
                            shell.allowed_commands.push(cmd_str);
                        }
                    }
                }
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "command": command,
                        "agent_id": ctx.agent_id,
                        "message": format!("`{}` を許可コマンドに追加しました", command),
                    })),
                    error: None,
                }
            }
            Ok(false) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "command": command,
                    "agent_id": ctx.agent_id,
                    "message": format!("`{}` はすでに許可コマンドに登録されています", command),
                    "already_exists": true,
                })),
                error: None,
            },
            Err(e) => {
                error!("add_agent_allowed_command failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("許可コマンドの追加に失敗: {e}")),
                }
            }
        }
    }

    pub(crate) fn execute_list_allowed_commands(
        &self,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::list_agent_allowed_commands(&conn, &ctx.agent_id) {
            Ok(commands) => {
                let count = commands.len();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "commands": commands,
                        "count": count,
                        "agent_id": ctx.agent_id,
                    })),
                    error: None,
                }
            }
            Err(e) => {
                error!("list_agent_allowed_commands failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("許可コマンドの取得に失敗: {e}")),
                }
            }
        }
    }

    pub(crate) fn execute_remove_allowed_command(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        if ctx.caller != GatewayCaller::Owner {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはオーナーのみ実行できます".to_string()),
            };
        }

        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("commandパラメータが必要です".to_string()),
                }
            }
        };

        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::remove_agent_allowed_command(&conn, &ctx.agent_id, command) {
            Ok(true) => {
                // Update in-memory tools_config
                drop(conn);
                if let Ok(mut cfg) = self.tools_config.write() {
                    if let Some(ref mut shell) = cfg.shell {
                        shell.allowed_commands.retain(|c| c != command);
                    }
                }
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "command": command,
                        "agent_id": ctx.agent_id,
                        "message": format!("`{}` を許可コマンドから削除しました", command),
                    })),
                    error: None,
                }
            }
            Ok(false) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "command": command,
                    "agent_id": ctx.agent_id,
                    "message": format!("`{}` は許可コマンドに登録されていませんでした", command),
                    "not_found": true,
                })),
                error: None,
            },
            Err(e) => {
                error!("remove_agent_allowed_command failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("許可コマンドの削除に失敗: {e}")),
                }
            }
        }
    }

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
