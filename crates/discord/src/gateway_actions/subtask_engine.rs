//! サブタスクエンジン操作 (spawn_subtask, cancel_subtask, report_progress)

use std::sync::Arc;

use chrono::Utc;
use opencrab_gateway::GatewayActionResult;
use serde_json::json;
use uuid::Uuid;

use super::webhook::{
    self, DeliveryBatch, LifecycleMeta, WebhookConfig, WebhookResolution, WebhookSource,
};
use super::{ArcLlmClient, DiscordGatewayActions, SpawnedSubtask};

impl DiscordGatewayActions {
    pub(crate) async fn execute_spawn_subtask(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let task = match args["task"].as_str() {
            Some(t) => t.to_string(),
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("spawn_subtask: 'task' argument is required".to_string()),
                };
            }
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(1800) as u64;
        let parent_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let parent_depth = args["__depth"].as_u64().unwrap_or(0) as u32;
        let agent_id = args["__agent_id"]
            .as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        let subtask_id = Uuid::new_v4().to_string();
        let sub_session_id = format!("subtask-{}", subtask_id);
        let spawned_at = Utc::now().to_rfc3339();
        let started_instant = std::time::Instant::now();
        let depth = parent_depth + 1;

        // Subtask lifecycle webhook を固定順序で解決する（explicit > tool > agent > global > env）。
        // db lock は解決の間だけ握り、await をまたがない。
        let resolution = {
            let conn = self.db.lock().unwrap();
            webhook::resolve_subtask_webhook(
                &conn,
                &agent_id,
                "spawn_subtask",
                args,
                self.default_subtask_webhook.as_ref(),
            )
        };

        // 解決結果を webhook 設定 + 可視性メタへ写像する。
        let (webhook, webhook_source, webhook_status): (
            Option<WebhookConfig>,
            Option<WebhookSource>,
            &'static str,
        ) = match resolution {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                // 検証失敗 → spawn しない。raw url はどこにも出さない。
                return GatewayActionResult {
                    success: false,
                    error: Some(format!("{code}: {message}")),
                    data: Some(json!({
                        "webhook_source": source.as_str(),
                        "webhook_status": "error",
                        "webhook_error": message,
                    })),
                };
            }
            WebhookResolution::Use { config, source } => (Some(config), Some(source), "ok"),
            WebhookResolution::Disabled { source } => (None, Some(source), "disabled"),
            WebhookResolution::None => (None, None, "none"),
        };
        let webhook_redacted_url = webhook
            .as_ref()
            .map(|cfg| webhook::redact_webhook_url(&cfg.url));
        let webhook_source_str: Option<&'static str> = webhook_source.map(|s| s.as_str());

        let label = args["label"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| task.chars().take(50).collect::<String>());

        // give-up 時に親セッションログへ 1 件記録する sink を構築する。
        let giveup_sink: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>> =
            if webhook.is_some() && !parent_session_id.is_empty() {
                let db_sink = self.db.clone();
                let agent_sink = agent_id.clone();
                let parent_sink = parent_session_id.clone();
                let subtask_sink = subtask_id.clone();
                let sub_session_sink = sub_session_id.clone();
                let redacted_sink = webhook_redacted_url.clone().unwrap_or_default();
                Some(std::sync::Arc::new(move |error: &str| {
                    if let Ok(conn) = db_sink.lock() {
                        webhook::record_webhook_delivery_failure(
                            &conn,
                            &agent_sink,
                            &parent_sink,
                            &subtask_sink,
                            &sub_session_sink,
                            &redacted_sink,
                            error,
                        );
                    }
                }))
            } else {
                None
            };

        // 同一 run の lifecycle delivery を直列化する worker を起動し、started を送る。
        let webhook_tx: Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>> =
            if let Some(cfg) = &webhook {
                let tx =
                    webhook::spawn_run_worker_with_sink(self.webhook_client.clone(), giveup_sink);
                if cfg.wants("started") {
                    let meta = LifecycleMeta {
                        label: label.clone(),
                        run_id: subtask_id.clone(),
                        session_key: sub_session_id.clone(),
                    };
                    let messages =
                        webhook::build_started_messages(&meta, &task, webhook::DISCORD_CHUNK_LIMIT);
                    let _ = tx.send(DeliveryBatch {
                        url: cfg.url.clone(),
                        messages,
                    });
                }
                Some(tx)
            } else {
                None
            };

        // 一般ツール/コマンド活動（activity family）のデフォルト webhook があれば、
        // sub-engine の executor に ToolEventSink を挿し tool_call_* を配送する。
        // env/config fallback は使わない（activity kind の DB 行のみ）。
        let has_activity = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::list_agent_webhook_config(&conn, Some(&agent_id), false)
                .map(|rows| rows.iter().any(|r| r.kind == "activity"))
                .unwrap_or(false)
        };
        let tool_event_sink: Option<Arc<dyn opencrab_actions::ToolEventSink>> = if has_activity {
            let tx = webhook_tx.clone().unwrap_or_else(|| {
                webhook::spawn_run_worker_with_sink(self.webhook_client.clone(), None)
            });
            Some(Arc::new(WebhookToolEventSink {
                db: self.db.clone(),
                agent_id: agent_id.clone(),
                tx,
                max_chars: 1500,
                counter: std::sync::atomic::AtomicUsize::new(0),
                cap: 200,
            }))
        } else {
            None
        };

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
                        "webhook_source": webhook_source_str,
                        "webhook_status": webhook_status,
                        "webhook_redacted_url": webhook_redacted_url,
                    })
                    .to_string(),
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
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("spawn_subtask: no LLM client available".to_string()),
                };
            }
        };

        let ws_path = self.workspace_root.join(&agent_id);
        std::fs::create_dir_all(&ws_path).ok();
        let workspace = match opencrab_core::workspace::Workspace::from_root(&ws_path) {
            Ok(w) => w,
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("spawn_subtask: workspace error: {e}")),
                };
            }
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
        let sub_executor = {
            let exec =
                opencrab_actions::BridgedExecutor::new(sub_dispatcher, sub_ctx).with_depth(depth);
            match tool_event_sink {
                Some(sink) => exec.with_tool_event_sink(sink),
                None => exec,
            }
        };

        let mut sub_engine = opencrab_core::SkillEngine::new(
            Box::new(ArcLlmClient(llm_client)),
            Box::new(sub_executor),
            usize::MAX,
        );

        if let (Some(cfg), Some(tx)) = (&webhook, &webhook_tx) {
            if cfg.wants("progress") {
                let progress_url = cfg.url.clone();
                let progress_tx = tx.clone();
                let progress_subtask_id = subtask_id.clone();
                let progress_session_id = sub_session_id.clone();
                sub_engine.set_on_tool_call(move |assistant_content, tool_calls_json| {
                    let detail = summarize_tool_calls(&assistant_content, &tool_calls_json);
                    let msg = webhook::build_progress_message(
                        &progress_subtask_id,
                        &progress_session_id,
                        &detail,
                    );
                    let _ = progress_tx.send(DeliveryBatch {
                        url: progress_url.clone(),
                        messages: vec![msg],
                    });
                });

                let progress_url = cfg.url.clone();
                let progress_tx = tx.clone();
                let progress_subtask_id = subtask_id.clone();
                let progress_session_id = sub_session_id.clone();
                sub_engine.set_on_tool_result(
                    move |_tool_call_id, tool_name, result_json, is_error| {
                        let status = if is_error { "failed" } else { "completed" };
                        let preview: String = result_json.chars().take(500).collect();
                        let detail = format!("tool `{tool_name}` {status}\n{preview}");
                        let msg = webhook::build_progress_message(
                            &progress_subtask_id,
                            &progress_session_id,
                            &detail,
                        );
                        let _ = progress_tx.send(DeliveryBatch {
                            url: progress_url.clone(),
                            messages: vec![msg],
                        });
                    },
                );
            }
        }

        // エージェントの personality と instructions を DB から取得
        let (agent_personality, agent_instructions) = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::get_agent(&conn, &agent_id)
                .ok()
                .flatten()
                .map(|a| (a.personality.unwrap_or_default(), a.instructions))
                .unwrap_or_default()
        };

        // System prompt for the sub-engine.
        let personality_section = if !agent_personality.is_empty() {
            format!("{}\n\n", agent_personality)
        } else {
            String::new()
        };
        let instructions_section = if !agent_instructions.is_empty() {
            format!("\n\n## Instructions\n{}", agent_instructions)
        } else {
            String::new()
        };
        let sub_system_prompt = format!(
            "{personality_section}\
             あなたはサブエンジンとして起動されています。\n\
             - subtask_id: {subtask_id}\n\
             - depth: {depth}\n\
             - Discordへの直接送信は禁止されています\n\
             - 進捗報告は report_progress を使ってください\n\
             - タスク完了時はテキストで結果を返してください（Discord送信はメインエンジンが行います）\n\n\
             You are a sub-engine executing a delegated task.\
             {instructions_section}"
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
        let webhook_clone = webhook.clone();
        let webhook_tx_clone = webhook_tx.clone();

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
                        })
                        .to_string(),
                        speaker_id: None,
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                }
            }

            // Emit terminal lifecycle webhook (completed / failed / timed_out).
            if let (Some(cfg), Some(tx)) = (&webhook_clone, &webhook_tx_clone) {
                let status = webhook::exit_reason_to_status(&exit_reason);
                if cfg.wants(status) {
                    let duration_ms = started_instant.elapsed().as_millis() as u64;
                    let msg = webhook::build_terminal_message(
                        status,
                        &subtask_id_clone,
                        &sub_session_id_clone,
                        Some(duration_ms),
                        &result_text,
                    );
                    let _ = tx.send(DeliveryBatch {
                        url: cfg.url.clone(),
                        messages: vec![msg],
                    });
                }
            }

            // Remove from registry.
            subtask_registry_clone.remove(&subtask_id_clone);

            // Call completion callback if registered.
            if let Some(cb) = completion_registry_clone.get(&parent_session_clone) {
                cb(
                    subtask_id_clone.clone(),
                    result_text.clone(),
                    exit_reason.clone(),
                );
            }
        });

        let abort_handle = join_handle.abort_handle();
        self.subtask_registry.insert(
            subtask_id.clone(),
            SpawnedSubtask {
                abort_handle,
                session_id: sub_session_id.clone(),
                parent_session_id: parent_session_id.clone(),
                spawned_at: spawned_at.clone(),
                agent_id: agent_id.clone(),
                webhook: webhook.clone(),
                webhook_tx: webhook_tx.clone(),
                started_instant,
            },
        );

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "status": "spawned",
                "subtask_id": subtask_id,
                "session_id": sub_session_id,
                "spawned_at": spawned_at,
                "webhook_source": webhook_source_str,
                "webhook_redacted_url": webhook_redacted_url,
                "webhook_status": webhook_status,
                "webhook_error": serde_json::Value::Null,
            })),
            error: None,
        }
    }

    pub(crate) fn execute_cancel_subtask(&self, args: &serde_json::Value) -> GatewayActionResult {
        let subtask_id = match args["subtask_id"].as_str() {
            Some(id) => id.to_string(),
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("cancel_subtask: 'subtask_id' is required".to_string()),
                };
            }
        };

        match self.subtask_registry.remove(&subtask_id) {
            Some((_, subtask)) => {
                subtask.abort_handle.abort();

                // Emit aborted lifecycle webhook. アボートで spawned closure は
                // 中断されるため terminal completed/failed は来ない → ここが唯一の終端。
                if let (Some(cfg), Some(tx)) = (&subtask.webhook, &subtask.webhook_tx) {
                    if cfg.wants("aborted") {
                        let duration_ms = subtask.started_instant.elapsed().as_millis() as u64;
                        let msg = webhook::build_terminal_message(
                            "aborted",
                            &subtask_id,
                            &subtask.session_id,
                            Some(duration_ms),
                            "cancelled by request",
                        );
                        let _ = tx.send(DeliveryBatch {
                            url: cfg.url.clone(),
                            messages: vec![msg],
                        });
                    }
                }

                // Write subtask_cancelled to parent session log.
                let parent_session_id = subtask.parent_session_id.clone();
                if !parent_session_id.is_empty() {
                    if let Ok(conn) = self.db.lock() {
                        let task_description =
                            opencrab_db::queries::get_session(&conn, &subtask.session_id)
                                .ok()
                                .flatten()
                                .map(|session| {
                                    session
                                        .theme
                                        .strip_prefix("Subtask: ")
                                        .unwrap_or(&session.theme)
                                        .to_string()
                                })
                                .unwrap_or_default();
                        let log = opencrab_db::queries::SessionLogRow {
                            id: None,
                            agent_id: subtask.agent_id.clone(),
                            session_id: parent_session_id.clone(),
                            log_type: "tool_cancelled".to_string(),
                            content: format!("subtask '{}' was cancelled", task_description),
                            speaker_id: None,
                            turn_number: None,
                            metadata_json: Some(
                                serde_json::json!({
                                    "tool_call_id": subtask_id,
                                    "tool_name": "spawn_subtask",
                                    "task": task_description,
                                })
                                .to_string(),
                            ),
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
                error: Some(format!(
                    "cancel_subtask: subtask '{}' not found",
                    subtask_id
                )),
            },
        }
    }

    pub(crate) async fn execute_report_progress(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let message = match args["message"].as_str() {
            Some(m) => m.to_string(),
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("report_progress: 'message' is required".to_string()),
                };
            }
        };
        let current_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let subtask_id_arg = args
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = args["__agent_id"]
            .as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        let subtask_entry = if !subtask_id_arg.is_empty() {
            self.subtask_registry
                .get(&subtask_id_arg)
                .map(|entry| (subtask_id_arg.clone(), entry.clone()))
        } else {
            self.subtask_registry
                .iter()
                .find(|entry| entry.session_id == current_session_id)
                .map(|entry| (entry.key().clone(), entry.value().clone()))
        };
        let subtask_id = subtask_entry
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or(subtask_id_arg);
        let parent_session_id = subtask_entry
            .as_ref()
            .map(|(_, subtask)| subtask.parent_session_id.clone())
            .unwrap_or_else(|| current_session_id.clone());

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
                    })
                    .to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }

        if let Some((resolved_subtask_id, subtask)) = &subtask_entry {
            if let (Some(cfg), Some(tx)) = (&subtask.webhook, &subtask.webhook_tx) {
                if cfg.wants("progress") {
                    let msg = webhook::build_progress_message(
                        resolved_subtask_id,
                        &subtask.session_id,
                        &message,
                    );
                    let _ = tx.send(DeliveryBatch {
                        url: cfg.url.clone(),
                        messages: vec![msg],
                    });
                }
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

fn summarize_tool_calls(assistant_content: &str, tool_calls_json: &str) -> String {
    let mut names = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(tool_calls_json) {
        if let Some(calls) = value.as_array() {
            for call in calls {
                if let Some(name) = call.get("name").and_then(|v| v.as_str()) {
                    names.push(format!("`{name}`"));
                }
            }
        }
    }
    let tools = if names.is_empty() {
        "tool call".to_string()
    } else {
        names.join(", ")
    };
    let preview: String = assistant_content.trim().chars().take(500).collect();
    if preview.is_empty() {
        format!("calling {tools}")
    } else {
        format!("calling {tools}\n{preview}")
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

/// activity family のデフォルト webhook へ tool_call_* を配送する sink。
/// イベントごとに resolve_activity_webhook で宛先を解決（tool > agent > global）し、
/// build_tool_event_message で redaction + クランプしてから送る。
struct WebhookToolEventSink {
    db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    agent_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    max_chars: usize,
    counter: AtomicUsize,
    cap: usize,
}

impl opencrab_actions::ToolEventSink for WebhookToolEventSink {
    fn on_event(&self, ev: &opencrab_actions::ToolEvent<'_>) {
        use opencrab_actions::ToolEventStatus;
        let (event_name, status) = match ev.status {
            ToolEventStatus::Started => ("tool_call_started", "started"),
            ToolEventStatus::Completed => ("tool_call_completed", "completed"),
            ToolEventStatus::Failed => ("tool_call_failed", "failed"),
            ToolEventStatus::Rejected => ("tool_call_rejected", "rejected"),
        };
        let resolution = {
            let conn = match self.db.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            webhook::resolve_activity_webhook(&conn, &self.agent_id, ev.tool_name)
        };
        let cfg = match resolution {
            WebhookResolution::Use { config, .. } => config,
            _ => return,
        };
        if !cfg.wants(event_name) {
            return;
        }
        // per-run の暴走ガード（超過分は 1 通だけ抑制サマリ）。
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        if n == self.cap {
            let _ = self.tx.send(DeliveryBatch {
                url: cfg.url.clone(),
                messages: vec![format!(
                    "(+ further tool events suppressed after {} this run)",
                    self.cap
                )],
            });
            return;
        }
        if n > self.cap {
            return;
        }
        let mut view = webhook::ToolEventView {
            event: event_name.to_string(),
            tool_name: ev.tool_name.to_string(),
            tool_call_id: ev.tool_call_id.to_string(),
            depth: ev.depth,
            status: status.to_string(),
            duration_ms: ev.duration_ms,
            max_chars: self.max_chars,
            ..Default::default()
        };
        view.args_summary = summarize_tool_args(ev.tool_name, ev.args);
        match ev.status {
            ToolEventStatus::Completed | ToolEventStatus::Failed => {
                if ev.tool_name == "execute_shell" {
                    if let Some(data) = ev.result {
                        let s = webhook::summarize_shell_result(data);
                        view.exit_code = s.exit_code;
                        view.stdout_summary = s.stdout_summary;
                        view.stderr_summary = s.stderr_summary;
                        view.truncated = s.truncated;
                    }
                } else if let Some(e) = ev.error {
                    view.result_summary = Some(e.to_string());
                } else if let Some(data) = ev.result {
                    view.result_summary = Some(short_json_preview(data));
                }
            }
            ToolEventStatus::Rejected => {
                view.rejection_reason = ev.error.map(|s| s.to_string());
            }
            ToolEventStatus::Started => {}
        }
        let messages = webhook::build_tool_event_message(&view);
        let _ = self.tx.send(DeliveryBatch {
            url: cfg.url.clone(),
            messages,
        });
    }
}

/// ツール引数の短い要約（execute_shell はコマンドを優先）。redaction は整形側で行う。
fn summarize_tool_args(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name == "execute_shell" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            let short: String = cmd.chars().take(300).collect();
            return Some(format!("cmd: `{short}`"));
        }
    }
    let s = args.to_string();
    if s == "null" || s == "{}" {
        return None;
    }
    Some(s.chars().take(300).collect())
}

/// 非 shell ツールの result の短い preview。
fn short_json_preview(data: &serde_json::Value) -> String {
    data.to_string().chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_activity(conn: &rusqlite::Connection) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "agent".to_string(),
            agent_id: "a1".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: "https://discord.com/api/webhooks/1/tok".to_string(),
            events_json: None,
            enabled: true,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    #[test]
    fn test_webhook_tool_event_sink_redacts_shell_output() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = Arc::new(std::sync::Mutex::new(conn));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        };
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({
            "exit_code": 0,
            "stdout": "leaked API_KEY=supersecretvalue here",
            "truncated": false
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&result),
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("a batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_completed"));
        assert!(msg.contains("exit_code"));
        assert!(!msg.contains("supersecretvalue"), "secret leaked: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_no_activity_row_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = Arc::new(std::sync::Mutex::new(conn));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        };
        let args = serde_json::json!({});
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        assert!(rx.try_recv().is_err(), "no activity row -> nothing sent");
    }
}
