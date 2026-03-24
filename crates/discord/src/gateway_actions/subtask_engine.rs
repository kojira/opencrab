//! サブタスクエンジン操作 (spawn_subtask, cancel_subtask, report_progress)

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;
use opencrab_gateway::GatewayActionResult;

use super::{ArcLlmClient, DiscordGatewayActions, SpawnedSubtask};

impl DiscordGatewayActions {
    pub(crate) async fn execute_spawn_subtask(&self, args: &serde_json::Value) -> GatewayActionResult {
        let task = match args["task"].as_str() {
            Some(t) => t.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("spawn_subtask: 'task' argument is required".to_string()),
            },
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(1800) as u64;
        let parent_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let parent_depth = args["__depth"].as_u64().unwrap_or(0) as u32;
        let agent_id = args["__agent_id"].as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        let subtask_id = Uuid::new_v4().to_string();
        let sub_session_id = format!("subtask-{}", subtask_id);
        let spawned_at = Utc::now().to_rfc3339();
        let depth = parent_depth + 1;

        // Create the sub-session in the DB.
        {
            let conn = self.db.lock().unwrap();
            let meta = serde_json::json!({
                "parent_session_id": parent_session_id,
                "depth": depth,
                "subtask_id": subtask_id,
            });
            let session = opencrab_db::queries::SessionRow {
                id: sub_session_id.clone(),
                mode: "subtask".to_string(),
                theme: format!("Subtask: {}", &task.chars().take(50).collect::<String>()),
                phase: "active".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!([&agent_id]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: Some(meta.to_string()),
            };
            opencrab_db::queries::insert_session(&conn, &session).ok();

            // Write subtask_spawned to parent session log.
            if !parent_session_id.is_empty() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: serde_json::json!({
                        "type": "subtask_spawned",
                        "subtask_id": subtask_id,
                        "session_id": sub_session_id,
                        "spawned_at": spawned_at,
                    }).to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }

        // Build sub-engine context.
        let llm_client = match self.llm_client.clone() {
            Some(c) => c,
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("spawn_subtask: no LLM client available".to_string()),
            },
        };

        let ws_path = self.workspace_root.join(&agent_id);
        std::fs::create_dir_all(&ws_path).ok();
        let workspace = match opencrab_core::workspace::Workspace::from_root(&ws_path) {
            Ok(w) => w,
            Err(e) => return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("spawn_subtask: workspace error: {e}")),
            },
        };

        let sub_ctx = opencrab_actions::ActionContext {
            caller: opencrab_actions::CallerIdentity::Agent,
            agent_id: agent_id.clone(),
            agent_name: agent_id.clone(),
            session_id: Some(sub_session_id.clone()),
            db: self.db.clone(),
            workspace: Arc::new(workspace),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            model_override: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("subtask".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
                default_model: self.default_model.clone(),
                active_model: None,
                available_providers: vec![],
                gateway: "subtask".to_string(),
            })),
        };

        let mut sub_dispatcher = opencrab_actions::ActionDispatcher::new();
        let tools_cfg = self.tools_config.read().unwrap().clone();
        opencrab_actions::register_tools_from_config(&tools_cfg, &mut sub_dispatcher);
        let sub_executor = opencrab_actions::BridgedExecutor::new(sub_dispatcher, sub_ctx)
            .with_depth(depth);

        let sub_engine = opencrab_core::SkillEngine::new(
            Box::new(ArcLlmClient(llm_client)),
            Box::new(sub_executor),
            usize::MAX,
        );

        // System prompt for the sub-engine.
        let sub_system_prompt = format!(
            "あなたはサブエンジンとして起動されています。\n\
             - subtask_id: {subtask_id}\n\
             - depth: {depth}\n\
             - Discordへの直接送信は禁止されています\n\
             - 進捗報告は report_progress を使ってください\n\
             - タスク完了時はテキストで結果を返してください（Discord送信はメインエンジンが行います）\n\n\
             You are a sub-engine executing a delegated task."
        );

        // Clone for the spawned task.
        let db_clone = self.db.clone();
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let sub_session_id_clone = sub_session_id.clone();
        let agent_id_clone = agent_id.clone();
        let completion_registry_clone = self.completion_registry.clone();
        let subtask_registry_clone = self.subtask_registry.clone();
        let default_model_clone = self.default_model.clone();

        let join_handle = tokio::spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                sub_engine.run_with_model_override(
                    &sub_system_prompt,
                    &task,
                    &default_model_clone,
                    None,
                    &[],
                ),
            )
            .await;

            let (exit_reason, result_text) = match result {
                Ok(Ok(engine_result)) => {
                    let exit_reason = if engine_result.stopped_by_limit {
                        "stopped_by_limit"
                    } else {
                        "completed"
                    };
                    (exit_reason.to_string(), engine_result.response)
                }
                Ok(Err(e)) => ("error".to_string(), format!("Error: {e}")),
                Err(_) => ("timeout".to_string(), "Subtask timed out.".to_string()),
            };

            // Write subtask_completed to parent session log.
            if !parent_session_clone.is_empty() {
                if let Ok(conn) = db_clone.lock() {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id_clone.clone(),
                        session_id: parent_session_clone.clone(),
                        log_type: "system".to_string(),
                        content: serde_json::json!({
                            "type": "subtask_completed",
                            "subtask_id": subtask_id_clone,
                            "session_id": sub_session_id_clone,
                            "exit_reason": exit_reason,
                            "result": result_text,
                        }).to_string(),
                        speaker_id: None,
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                }
            }

            // Remove from registry.
            subtask_registry_clone.remove(&subtask_id_clone);

            // Call completion callback if registered.
            if let Some(cb) = completion_registry_clone.get(&parent_session_clone) {
                cb(subtask_id_clone.clone(), result_text.clone(), exit_reason.clone());
            }
        });

        let abort_handle = join_handle.abort_handle();
        self.subtask_registry.insert(subtask_id.clone(), SpawnedSubtask {
            abort_handle,
            session_id: sub_session_id.clone(),
            parent_session_id: parent_session_id.clone(),
            spawned_at: spawned_at.clone(),
            agent_id: agent_id.clone(),
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "status": "spawned",
                "subtask_id": subtask_id,
                "session_id": sub_session_id,
                "spawned_at": spawned_at,
            })),
            error: None,
        }
    }

    pub(crate) fn execute_cancel_subtask(&self, args: &serde_json::Value) -> GatewayActionResult {
        let subtask_id = match args["subtask_id"].as_str() {
            Some(id) => id.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("cancel_subtask: 'subtask_id' is required".to_string()),
            },
        };

        match self.subtask_registry.remove(&subtask_id) {
            Some((_, subtask)) => {
                subtask.abort_handle.abort();

                // Write subtask_cancelled to parent session log.
                let parent_session_id = subtask.parent_session_id.clone();
                if !parent_session_id.is_empty() {
                    if let Ok(conn) = self.db.lock() {
                        let task_description = opencrab_db::queries::get_session(&conn, &subtask.session_id)
                            .ok()
                            .flatten()
                            .map(|session| {
                                session.theme
                                    .strip_prefix("Subtask: ")
                                    .unwrap_or(&session.theme)
                                    .to_string()
                            })
                            .unwrap_or_default();
                        let log = opencrab_db::queries::SessionLogRow {
                            id: None,
                            agent_id: subtask.agent_id.clone(),
                            session_id: parent_session_id.clone(),
                            log_type: "system".to_string(),
                            content: serde_json::json!({
                                "type": "subtask_cancelled",
                                "subtask_id": subtask_id,
                                "session_id": subtask.session_id,
                                "task": task_description,
                            }).to_string(),
                            speaker_id: None,
                            turn_number: None,
                            metadata_json: None,
                            created_at: None,
                        };
                        opencrab_db::queries::insert_session_log(&conn, &log).ok();
                    }
                }

                GatewayActionResult {
                    success: true,
                    data: Some(json!({"cancelled": true, "subtask_id": subtask_id})),
                    error: None,
                }
            }
            None => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("cancel_subtask: subtask '{}' not found", subtask_id)),
            },
        }
    }

    pub(crate) async fn execute_report_progress(&self, args: &serde_json::Value) -> GatewayActionResult {
        let message = match args["message"].as_str() {
            Some(m) => m.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("report_progress: 'message' is required".to_string()),
            },
        };
        let parent_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let subtask_id = args.get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = args["__agent_id"].as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        // Write progress to parent session log.
        if !parent_session_id.is_empty() {
            if let Ok(conn) = self.db.lock() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: serde_json::json!({
                        "type": "subtask_progress",
                        "subtask_id": subtask_id,
                        "message": message,
                        "timestamp": Utc::now().to_rfc3339(),
                    }).to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }

        // Debounce: wait 3 seconds then trigger main engine re-invocation via completion callback.
        let completion_registry_clone = self.completion_registry.clone();
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let message_clone = message.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Some(cb) = completion_registry_clone.get(&parent_session_clone) {
                cb(subtask_id_clone, message_clone, "progress".to_string());
            }
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({"reported": true, "message": message})),
            error: None,
        }
    }

}
