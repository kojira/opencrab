//! サブタスクエンジン操作 (spawn_subtask, cancel_subtask, report_progress)

use std::sync::Arc;

use chrono::Utc;
use opencrab_gateway::{GatewayActionResult, GatewayCallContext};
use serde_json::json;
use uuid::Uuid;

use super::subtask_webhook::reject;
use super::webhook::{
    self, DeliveryBatch, LifecycleMeta, WebhookConfig, WebhookResolution, WebhookSource,
};
use super::{ArcLlmClient, DiscordGatewayActions, SpawnedSubtask};
use crate::message_loop::{parse_discord_session, LoopEvent};

/// sub-engine に許可する gateway アクションの許可リスト（#63）。
///
/// bridge の DISCORD_ACTIONS depth ゲートは 28 アクション中 5 つしかブロックしないため、
/// 素の DiscordGatewayActions を接続すると、ハンドラ側ゲートの無いアクション
/// （send_ui / discord_channel_config / discord_create_channel / update_memory_index_config
/// 等）が depth>=1 に開放されてしまう。deny-list に頼らず、ここで明示的に許可した
/// アクションだけを sub-engine から到達可能にする。
///
/// spawn_subtask は意図的に含めない: ネスト spawn は従来も（gateway 未接続のため）
/// 不可能だった現状維持。ネストを有効化する場合は bridge の MAX_DEPTH ゲートではなく
/// この許可リストが実効ゲートである点に注意。
const SUB_ENGINE_ALLOWED_ACTIONS: &[&str] = &["report_progress"];

/// sub-engine 専用の最小権限 gateway。許可リストのアクションだけを親実装へ委譲する。
///
/// `inner` は DiscordGatewayActions の共有クローン（subtask_registry / progress_debounce /
/// event_tx / db を親と共有）なので、report_progress の registry 照合・デバウンス・
/// 完了イベント送信は親経由の呼び出しと同一に動く。
pub(crate) struct SubEngineGatewayActions {
    inner: DiscordGatewayActions,
}

impl SubEngineGatewayActions {
    pub(crate) fn new(inner: DiscordGatewayActions) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl opencrab_gateway::GatewayActions for SubEngineGatewayActions {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| SUB_ENGINE_ALLOWED_ACTIONS.contains(&d.name.as_str()))
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        if SUB_ENGINE_ALLOWED_ACTIONS.contains(&name) {
            return self.inner.execute(name, args, ctx).await;
        }
        // 実在するが許可外 → 権限拒否（rejected: マーカー）。
        // 未知の名前 → 通常の失敗（幻覚ツール名を Rejected に誤分類させない）。
        if self.inner.definitions().iter().any(|d| d.name == name) {
            reject(format!("action '{name}' is not available in sub-engines"))
        } else {
            GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            }
        }
    }
}

/// parent_session_id から routing 情報を復元して SubtaskCompleted イベントを送る（#39）。
///
/// 旧 completion_registry はメッセージごとに完了クロージャを登録していたが、
/// クロージャのキャプチャ値は全て session_id（`discord-{agent}-{guild}-{channel}`）から
/// 導出可能だったため、パーサ + イベントループへの直接送信に置き換えた。
/// event_tx 未設定（イベントループの無い構築、例: 一発呼びの API 経路）や
/// Discord 形式でない session は、旧実装でレジストリ未登録だった場合と同様に
/// 発火しない（warn のみ）。
fn send_subtask_completed_event(
    event_tx: Option<&tokio::sync::mpsc::UnboundedSender<LoopEvent>>,
    parent_session_id: &str,
    agent_id: &str,
    subtask_id: String,
    result: String,
    exit_reason: String,
) {
    let Some(tx) = event_tx else {
        tracing::debug!(
            session_id = %parent_session_id,
            "subtask completion: event_tx not configured, skipping main-engine notification"
        );
        return;
    };
    let Some((guild_id, channel_id)) = parse_discord_session(parent_session_id) else {
        // 非 Discord の親セッション（heartbeat-* / subtask-* のネスト等）は正常系。
        // 旧レジストリ実装でも未登録で発火しなかったため、debug に留める。
        tracing::debug!(
            session_id = %parent_session_id,
            "subtask completion: parent session is not a discord session, skipping main-engine notification"
        );
        return;
    };
    let is_dm = guild_id.is_empty();
    let _ = tx.send(LoopEvent::SubtaskCompleted {
        session_id: parent_session_id.to_string(),
        agent_id: agent_id.to_string(),
        subtask_id,
        result,
        exit_reason,
        channel_id,
        channel_id_str: channel_id.to_string(),
        guild_id,
        is_dm,
    });
}

impl DiscordGatewayActions {
    pub(crate) async fn execute_spawn_subtask(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
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
        // セッション必須（fail-closed）: 完了通知・親ログの宛先が session_id に
        // 依存するため、不明なまま "" で進まず明示エラーにする（#36）。
        let parent_session_id = match ctx.session_id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "spawn_subtask はセッション文脈でのみ実行できます（session_id 不明）"
                            .to_string(),
                    ),
                };
            }
        };
        let parent_depth = ctx.depth;
        let agent_id = ctx.agent_id.clone();

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
                emit_activity_diagnostic(
                    self.db.clone(),
                    self.webhook_client.clone(),
                    &agent_id,
                    "spawn_subtask",
                    "webhook_resolution_error",
                    &format!(
                        "spawn_subtask webhook resolution failed before execution: {code}: {message} (source: {})",
                        source.as_str()
                    ),
                    args,
                    None,
                );
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

        // 一般ツール/コマンド活動（activity family）のデフォルト webhook があるか。
        // 判定は webhook::has_activity_default に集約（resolve_activity_webhook と同じ
        // scope 集合: tool/agent/global の enabled な activity 行。env/config fallback なし）。
        let has_activity = {
            let conn = self.db.lock().unwrap();
            webhook::has_activity_default(&conn, &agent_id)
        };

        // 同一 run の配送を直列化する worker を 1 つだけ起動する。lifecycle（started/
        // completed/...）と tool_call_*（activity）を同じ tx に流すことで、両系統の
        // 送出順序を 1 本の worker で保証する（別 worker を立てて順序が崩れるのを防ぐ）。
        let webhook_tx: Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>> =
            if webhook.is_some() || has_activity {
                Some(webhook::spawn_run_worker_with_sink(
                    self.webhook_client.clone(),
                    giveup_sink,
                ))
            } else {
                None
            };

        if webhook_status != "ok" {
            emit_activity_diagnostic(
                self.db.clone(),
                self.webhook_client.clone(),
                &agent_id,
                "spawn_subtask",
                "webhook_resolution_diagnostic",
                &format!(
                    "spawn_subtask lifecycle webhook status is {webhook_status}; source={}",
                    webhook_source_str.unwrap_or("none")
                ),
                args,
                webhook_tx.as_ref(),
            );
        }

        // lifecycle の started を送る（同じ共有 worker 経由）。
        if let (Some(cfg), Some(tx)) = (&webhook, &webhook_tx) {
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
        }

        // activity webhook があれば、sub-engine の executor に ToolEventSink を挿し
        // tool_call_* を共有 worker 経由で配送する。
        let tool_event_sink: Option<Arc<dyn opencrab_actions::ToolEventSink>> =
            match (has_activity, &webhook_tx) {
                (true, Some(tx)) => Some(Arc::new(WebhookToolEventSink {
                    db: self.db.clone(),
                    agent_id: agent_id.clone(),
                    tx: tx.clone(),
                    max_chars: 1500,
                    counter: std::sync::atomic::AtomicUsize::new(0),
                    cap: 200,
                })),
                _ => None,
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

        // 実効モデルは1回だけ解決する（runtime_info と run_with_model_override で
        // 同じ値を使う — 呼び出し間に設定が変わっても不整合にしない）。
        let effective_model = self.effective_model(&agent_id);
        let ws_path = self.agent_workspace_root(&agent_id).join(&agent_id);
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
                default_model: effective_model.clone(),
                active_model: None,
                available_providers: vec![],
                gateway: "subtask".to_string(),
            })),
        };

        let mut sub_dispatcher = opencrab_actions::ActionDispatcher::new();
        let mut tools_cfg = self.tools_config.read().unwrap().clone();
        // このエージェント専用の許可コマンド（DB管理）をローカルコピーにのみマージする
        // （グローバル config に足すと他エージェントへ漏れる）。
        if let Ok(conn) = self.db.lock() {
            if let Ok(agent_cmds) =
                opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id)
            {
                if !agent_cmds.is_empty() {
                    let shell = tools_cfg
                        .shell
                        .get_or_insert_with(opencrab_actions::tools::ShellToolConfig::default);
                    for cmd in agent_cmds {
                        if !shell.allowed_commands.contains(&cmd) {
                            shell.allowed_commands.push(cmd);
                        }
                    }
                }
            }
        }
        opencrab_actions::register_tools_from_config(&tools_cfg, &mut sub_dispatcher);
        let sub_executor = {
            // 許可リストラッパ経由で gateway を接続する（#63）。これが無いと
            // system prompt が指示する report_progress が "Unknown action" で失敗する。
            let sub_gateway: Arc<dyn opencrab_gateway::GatewayActions> =
                Arc::new(SubEngineGatewayActions::new(self.clone()));
            let exec = opencrab_actions::BridgedExecutor::new(sub_dispatcher, sub_ctx)
                .with_depth(depth)
                .with_gateway_actions(sub_gateway);
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
             - 進捗報告は report_progress を使ってください（subtask_id 引数は省略可。省略時はこのサブタスクとして報告されます）\n\
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
        let event_tx_clone = self.event_tx.clone();
        let subtask_registry_clone = self.subtask_registry.clone();
        let default_model_clone = effective_model.clone();
        let webhook_clone = webhook.clone();
        let webhook_tx_clone = webhook_tx.clone();

        // 開始ゲート: 親がレジストリへ insert し終えるまでタスク本体を走らせない。
        // これが無いと、即座に失敗するサブタスクが親の insert より先に remove を実行し、
        // その後 insert が着地して「running のまま」のエントリがリークする。
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        let join_handle = tokio::spawn(async move {
            // insert 完了を待つ（送信側が drop された場合も先へ進む）。
            let _ = start_rx.await;

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
                    // harness 剪定メトリクス: sub-engine の run も XML フォールバック発火を
                    // agent_logs に記録する（server 側と同じ context キー。subtask だけ
                    // 計測から漏れると消し時の判断を誤る — docs/harness-inventory.md 参照）。
                    if engine_result.xml_fallback_parses > 0 {
                        if let Ok(conn) = db_clone.lock() {
                            let _ = opencrab_db::queries::insert_agent_log(
                                &conn,
                                &opencrab_db::queries::AgentLogRow {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    agent_id: Some(agent_id_clone.clone()),
                                    level: "info".to_string(),
                                    context: "harness.xml_fallback".to_string(),
                                    message: format!(
                                        "XML <function_calls> fallback fired {} time(s) (model: {default_model_clone}, subtask: {subtask_id_clone})",
                                        engine_result.xml_fallback_parses
                                    ),
                                    created_at: Some(chrono::Utc::now().to_rfc3339()),
                                },
                            );
                        }
                    }
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

            // メインエンジンへの完了通知（イベントループへ直接送信）。
            send_subtask_completed_event(
                event_tx_clone.as_ref(),
                &parent_session_clone,
                &agent_id_clone,
                subtask_id_clone,
                result_text,
                exit_reason,
            );
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

        // insert が完了したのでタスク本体の実行を許可する。
        let _ = start_tx.send(());

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

    pub(crate) fn execute_cancel_subtask(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
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

        // 認可（#64）: owner は常に許可。それ以外は「呼び出し元セッションが親」の
        // サブタスクのみキャンセルできる（自己/兄弟/他セッションのものは不可）。
        let authorized = |subtask: &SpawnedSubtask| -> bool {
            if ctx.caller == opencrab_gateway::GatewayCaller::Owner {
                return true;
            }
            matches!(ctx.session_id.as_deref(),
                Some(s) if !s.is_empty() && subtask.parent_session_id == s)
        };

        // remove_if は shard ロック下で述語を評価するため、「認可確認→削除」の間に
        // エントリが差し替わる TOCTOU が無い（所有権フィールドは insert 後不変）。
        match self
            .subtask_registry
            .remove_if(&subtask_id, |_, subtask| authorized(subtask))
        {
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
            None => {
                // remove_if の None は「不在」と「権限なし」の両方。エントリの所有権
                // フィールドは不変なので contains_key で区別できる。
                if self.subtask_registry.contains_key(&subtask_id) {
                    reject(format!(
                        "cancel_subtask: subtask '{subtask_id}' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）"
                    ))
                } else {
                    GatewayActionResult {
                        success: false,
                        data: None,
                        error: Some(format!(
                            "cancel_subtask: subtask '{}' not found",
                            subtask_id
                        )),
                    }
                }
            }
        }
    }

    pub(crate) async fn execute_report_progress(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
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
        // セッション必須（fail-closed）: 親セッションの解決が session_id に依存する（#36）。
        let current_session_id = match ctx.session_id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "report_progress はセッション文脈でのみ実行できます（session_id 不明）"
                            .to_string(),
                    ),
                };
            }
        };
        let subtask_id_arg = args
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = ctx.agent_id.clone();

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

        // 所有権ゲート（#64）: subtask_id は LLM 由来の引数なので、呼び出し元セッションの
        // サブタスク（自分自身 = entry.session_id 一致、または自分の子 =
        // entry.parent_session_id 一致）以外は拒否する。無検証だと他セッションへの
        // 進捗ログ書き込み・webhook 配送・メインエンジン再呼び出しを誘発できてしまう。
        if let Some((id, entry)) = &subtask_entry {
            if entry.session_id != current_session_id
                && entry.parent_session_id != current_session_id
            {
                return reject(format!(
                    "report_progress: subtask '{id}' は呼び出し元セッションのサブタスクではありません"
                ));
            }
        }

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

        // Debounce: 3秒待ってからメインエンジン再呼び出しを1回だけ発火する。
        // 世代カウンタで「最後の report_progress」だけが発火するようにし、バースト時に
        // 同数のLLM再呼び出し（コスト増・チャンネルスパム・イベントループ長時間ブロック）が
        // 起きるのを防ぐ。
        let my_generation = {
            let mut gen = self
                .progress_debounce
                .entry(parent_session_id.clone())
                .or_insert(0);
            *gen += 1;
            *gen
        };
        let event_tx_clone = self.event_tx.clone();
        let progress_debounce_clone = self.progress_debounce.clone();
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let agent_id_clone = agent_id.clone();
        let message_clone = message.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            // 自分より後に report_progress が来ていたら（世代が進んでいたら）発火しない。
            let is_latest = progress_debounce_clone
                .get(&parent_session_clone)
                .map(|g| *g == my_generation)
                .unwrap_or(false);
            if !is_latest {
                return;
            }
            progress_debounce_clone.remove(&parent_session_clone);
            send_subtask_completed_event(
                event_tx_clone.as_ref(),
                &parent_session_clone,
                &agent_id_clone,
                subtask_id_clone,
                message_clone,
                "progress".to_string(),
            );
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
                // 正準形状 {function:{name}} と旧形状 {name} の両方に対応する。
                let name = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .or_else(|| call.get("name").and_then(|v| v.as_str()));
                if let Some(name) = name {
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

/// depth0/メインエージェントの executor に挿す activity ツールイベント sink を構築する。
///
/// `agent_id` に対する有効な activity 行（agent scope または global `*`）が無ければ
/// `None` を返し、配送 worker も起動しない（best-effort・無駄なタスクを作らない）。
/// 返した sink は spawn_subtask の sub-engine 用 sink と同じ実体で、イベントごとに
/// `resolve_activity_webhook`（tool > agent > global）で宛先を解決し、
/// `build_tool_event_message` で整形（covered 経路ゆえ redaction せず、上限超過のみ
/// ロスレス chunk）してから送る。disabled/不正 URL は
/// 黙って下位へ fall through せず診断を残す（no-silent-fallback）。
///
/// メイン engine は spawn_subtask のような lifecycle webhook を持たないため、ここでは
/// 専用の run worker を 1 本だけ起動して tool_call_* を直列配送する。
pub fn spawn_activity_tool_event_sink(
    db: opencrab_db::Db,
    agent_id: &str,
) -> Option<Arc<dyn opencrab_actions::ToolEventSink>> {
    let has_activity = {
        let conn = db.lock().ok()?;
        webhook::has_activity_default(&conn, agent_id)
    };
    if !has_activity {
        return None;
    }
    let tx = webhook::spawn_run_worker_with_sink(reqwest::Client::new(), None);
    Some(Arc::new(WebhookToolEventSink {
        db,
        agent_id: agent_id.to_string(),
        tx,
        max_chars: 1500,
        counter: AtomicUsize::new(0),
        cap: 200,
    }))
}

fn emit_activity_diagnostic(
    db: opencrab_db::Db,
    client: reqwest::Client,
    agent_id: &str,
    tool_name: &str,
    diagnostic_event: &str,
    reason: &str,
    args: &serde_json::Value,
    existing_tx: Option<&tokio::sync::mpsc::UnboundedSender<DeliveryBatch>>,
) {
    let Some(batch) =
        build_activity_diagnostic_batch(&db, agent_id, tool_name, diagnostic_event, reason, args)
    else {
        return;
    };
    if let Some(tx) = existing_tx {
        let _ = tx.send(batch);
    } else {
        let tx = webhook::spawn_run_worker_with_sink(client, None);
        let _ = tx.send(batch);
    }
}

fn build_activity_diagnostic_batch(
    db: &opencrab_db::Db,
    agent_id: &str,
    tool_name: &str,
    diagnostic_event: &str,
    reason: &str,
    args: &serde_json::Value,
) -> Option<DeliveryBatch> {
    let resolution = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "webhook_audit",
                    agent_id = %agent_id,
                    tool = %tool_name,
                    event = %diagnostic_event,
                    error = %e,
                    "activity webhook diagnostic could not lock db"
                );
                return None;
            }
        };
        webhook::resolve_activity_webhook(&conn, agent_id, tool_name)
    };
    let cfg = match resolution {
        WebhookResolution::Use { config, .. } => config,
        WebhookResolution::Error {
            code,
            message,
            source,
        } => {
            tracing::warn!(
                target: "webhook_audit",
                agent_id = %agent_id,
                tool = %tool_name,
                event = %diagnostic_event,
                source = %source.as_str(),
                code = %code,
                reason = %message,
                "activity webhook diagnostic dropped because default resolution failed"
            );
            return None;
        }
        WebhookResolution::Disabled { source } => {
            tracing::warn!(
                target: "webhook_audit",
                agent_id = %agent_id,
                tool = %tool_name,
                event = %diagnostic_event,
                source = %source.as_str(),
                "activity webhook diagnostic dropped because default is disabled"
            );
            return None;
        }
        WebhookResolution::None => {
            tracing::warn!(
                target: "webhook_audit",
                agent_id = %agent_id,
                tool = %tool_name,
                event = %diagnostic_event,
                "activity webhook diagnostic dropped because no default is configured"
            );
            return None;
        }
    };
    if !cfg.wants(diagnostic_event) && !cfg.wants("tool_call_failed") {
        return None;
    }
    let view = webhook::ToolEventView {
        event: diagnostic_event.to_string(),
        tool_name: tool_name.to_string(),
        tool_call_id: "diagnostic".to_string(),
        depth: 0,
        status: "failed".to_string(),
        args_summary: summarize_tool_args(tool_name, args),
        result_summary: Some(reason.to_string()),
        max_chars: 1500,
        ..Default::default()
    };
    Some(DeliveryBatch {
        url: cfg.url,
        messages: webhook::build_tool_event_message(&view),
    })
}

/// activity family のデフォルト webhook へ tool_call_* を配送する sink。
/// イベントごとに resolve_activity_webhook で宛先を解決（tool > agent > global）し、
/// build_tool_event_message で整形（covered 経路ゆえ unredacted、上限超過のみロスレス
/// chunk）してから送る。
struct WebhookToolEventSink {
    db: opencrab_db::Db,
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
        // Use 以外（Error/Disabled/None）はイベントを配送しない（no-silent-fallback）。
        // 黙って捨てると原因が見えないため、raw URL/token を載せずに診断を残す。
        let cfg = match resolution {
            WebhookResolution::Use { config, .. } => config,
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                tracing::warn!(
                    target: "webhook_audit",
                    agent_id = %self.agent_id,
                    tool = %ev.tool_name,
                    event = %event_name,
                    source = %source.as_str(),
                    code = %code,
                    reason = %message,
                    "activity webhook resolution error; tool event dropped"
                );
                return;
            }
            WebhookResolution::Disabled { source } => {
                tracing::debug!(
                    target: "webhook_audit",
                    agent_id = %self.agent_id,
                    tool = %ev.tool_name,
                    event = %event_name,
                    source = %source.as_str(),
                    "activity webhook disabled; tool event dropped"
                );
                return;
            }
            WebhookResolution::None => {
                tracing::trace!(
                    target: "webhook_audit",
                    agent_id = %self.agent_id,
                    tool = %ev.tool_name,
                    event = %event_name,
                    "no activity webhook configured for this tool; tool event dropped"
                );
                return;
            }
        };
        if !cfg.wants(event_name) {
            // events フィルタで落ちた場合も黙って捨てず、原因が追えるよう診断を残す
            // （raw URL/token は載せない）。canonical な status 名で一致判定している。
            tracing::debug!(
                target: "webhook_audit",
                agent_id = %self.agent_id,
                tool = %ev.tool_name,
                event = %event_name,
                "activity tool event filtered out by configured events list; tool event dropped"
            );
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
                // 構造マーカー接頭辞は表示では落とし、人間可読の理由のみ残す。
                view.rejection_reason = ev.error.map(|s| {
                    s.strip_prefix(opencrab_actions::REJECTION_CODE_PREFIX)
                        .unwrap_or(s)
                        .to_string()
                });
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

/// ツール引数の要約（execute_shell はコマンドを優先）。
///
/// covered 経路（work-channel 出力）のため redaction も length クランプも行わず、
/// command / args 配列をそのまま返す（docs/design-webhook-output-lossless.md §2 P4）。
/// Discord のサイズ上限は `build_tool_event_message` がロスレス chunk で吸収する。
fn summarize_tool_args(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name == "execute_shell" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            // command 単体ではなく、実際に渡された引数（args 配列）も含めて要約する。
            // これがないと `echo hello world` が `cmd: echo` としか表示されず欠落する。
            let mut parts = vec![format!("cmd: `{cmd}`")];
            if let Some(arr) = args.get("args").and_then(|v| v.as_array()) {
                let items: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                if !items.is_empty() {
                    // JSON 配列として描画する（例: ["hello","webhook-args-test"]）。
                    parts.push(format!("args: {}", serde_json::Value::from(items)));
                }
            }
            // stdin は本文を出さず、存在とバイト数のみ示す（出力ではなく入力の要約）。
            if let Some(stdin) = args.get("stdin").and_then(|v| v.as_str()) {
                if !stdin.is_empty() {
                    parts.push(format!("stdin: {} bytes", stdin.len()));
                }
            }
            return Some(parts.join(" "));
        }
    }
    let s = args.to_string();
    if s == "null" || s == "{}" {
        return None;
    }
    Some(s)
}

/// 非 shell ツールの result の preview（covered 経路: redact もクランプもしない）。
fn short_json_preview(data: &serde_json::Value) -> String {
    data.to_string()
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
    fn test_webhook_tool_event_sink_preserves_shell_output_unredacted() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
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
        let msg = batch.messages.join("");
        assert!(msg.contains("tool_call_completed"));
        assert!(msg.contains("exit_code"));
        // covered 経路: stdout の secret はそのまま届く（masking しない）。
        assert!(
            msg.contains("API_KEY=supersecretvalue"),
            "secret stripped: {msg}"
        );
        assert!(!msg.contains("[REDACTED]"), "masking marker present: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_sends_failed_and_rejected() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "denied" });
        let failed_result = serde_json::json!({
            "exit_code": 2,
            "stderr": "API_KEY=supersecretvalue failed",
            "truncated": false
        });
        let failed = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "failed-call",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Failed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&failed_result),
            error: Some("command failed"),
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &failed);
        let rejected = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "rejected-call",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Rejected,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(1),
            args: &args,
            result: None,
            error: Some("permission denied"),
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &rejected);

        let failed_batch = rx.try_recv().expect("failed batch");
        let failed_msg = failed_batch.messages.join("");
        assert!(failed_msg.contains("tool_call_failed"));
        assert!(failed_msg.contains("exit_code"));
        // covered 経路: stderr の secret はそのまま届く（masking しない）。
        assert!(
            failed_msg.contains("API_KEY=supersecretvalue"),
            "secret stripped: {failed_msg}"
        );
        assert!(
            !failed_msg.contains("[REDACTED]"),
            "masking marker present: {failed_msg}"
        );
        let rejected_batch = rx.try_recv().expect("rejected batch");
        let rejected_msg = &rejected_batch.messages[0];
        assert!(rejected_msg.contains("tool_call_rejected"));
        assert!(rejected_msg.contains("permission denied"));
    }

    #[test]
    fn test_webhook_tool_event_sink_no_activity_row_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
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

    fn insert_activity_row(conn: &rusqlite::Connection, url: &str, enabled: bool) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "agent".to_string(),
            agent_id: "a1".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: url.to_string(),
            events_json: None,
            enabled,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    fn make_sink(
        db: opencrab_db::Db,
        tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    ) -> WebhookToolEventSink {
        WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        }
    }

    fn started_event<'a>(args: &'a serde_json::Value) -> opencrab_actions::ToolEvent<'a> {
        opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args,
            result: None,
            error: None,
        }
    }

    // ---- tool/command argument inclusion on activity webhook messages ----

    #[test]
    fn test_webhook_tool_event_sink_started_includes_command_args() {
        // started イベントにコマンド引数が含まれること（depth0 を想定）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "git status --short" });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("args:"), "args line missing: {msg}");
        assert!(msg.contains("git status --short"), "command missing: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_started_includes_command_and_args_array() {
        // E2E 再現: command `echo` と args `["hello","webhook-args-test"]` が
        // started イベントで両方描画されること（args 配列が欠落しない）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({
            "command": "echo",
            "args": ["hello", "webhook-args-test"]
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("echo"), "command missing: {msg}");
        assert!(msg.contains("hello"), "first arg missing: {msg}");
        assert!(
            msg.contains("webhook-args-test"),
            "second arg missing: {msg}"
        );
    }

    #[test]
    fn test_webhook_tool_event_sink_started_includes_non_shell_args() {
        // 非 shell ツールでも started に引数（JSON）が含まれる。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "path": "notes/todo.md", "limit": 10 });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "read_file",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0];
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("notes/todo.md"), "args missing: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_started_preserves_secret_args_unredacted() {
        // covered 経路: started の引数に含まれるシークレット（API キー / Discord webhook
        // URL）も masking せずそのまま届く（新要件 §2 P4 / AC4）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer sk-supersecretkeyvalue1234' https://discord.com/api/webhooks/999/leakedtokenvalue && export API_KEY=anothersupersecretvalue"
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = batch.messages.join("");
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        // every secret-like token survives unmodified, and no masking markers appear.
        assert!(
            !msg.contains("[REDACTED]"),
            "REDACTED marker present: {msg}"
        );
        assert!(
            !msg.contains("[redacted]"),
            "redacted marker present: {msg}"
        );
        assert!(
            msg.contains("sk-supersecretkeyvalue1234"),
            "api key stripped: {msg}"
        );
        assert!(
            msg.contains("https://discord.com/api/webhooks/999/leakedtokenvalue"),
            "webhook url stripped: {msg}"
        );
        assert!(
            msg.contains("API_KEY=anothersupersecretvalue"),
            "API_KEY value stripped: {msg}"
        );
    }

    #[test]
    fn test_webhook_tool_event_sink_long_args_chunked_losslessly() {
        // 長大な引数はクランプ（…）せず、Discord 上限内の part X/N へロスレス分割する。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let long_cmd = format!("echo {}", "word ".repeat(2_000));
        let args = serde_json::json!({ "command": long_cmd });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        assert!(batch.messages.len() > 1, "long args must split into parts");
        // each part within Discord hard limit, labelled in order.
        for (i, m) in batch.messages.iter().enumerate() {
            assert!(
                m.chars().count() <= 2000,
                "part exceeds limit: {}",
                m.chars().count()
            );
            assert!(
                m.starts_with(&format!("part {}/{}\n", i + 1, batch.messages.len())),
                "part marker/order wrong: {m}"
            );
        }
        // reconstruct -> all 2000 'word' tokens present, no ellipsis loss.
        let reconstructed: String = batch
            .messages
            .iter()
            .map(|m| m.splitn(2, '\n').nth(1).unwrap_or("").to_string())
            .collect();
        assert!(!reconstructed.contains('…'), "clamp ellipsis introduced");
        assert_eq!(reconstructed.matches("word").count(), 2_000, "lost args");
    }

    // ---- summarize_tool_args: unredacted, lossless ----

    #[test]
    fn test_summarize_tool_args_preserves_secrets_unredacted() {
        // covered 経路: 引数中の secret も masking/クランプせずそのまま残す。
        let secret = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd"; // 40 文字の英数字
        let prefix = "a ".repeat(145); // 290 文字
        let cmd = format!("{prefix}{secret}");
        let args = serde_json::json!({ "command": cmd });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.starts_with("cmd: `"), "summary: {summary}");
        assert!(summary.contains(secret), "secret stripped: {summary}");
        assert!(
            !summary.contains("[REDACTED]"),
            "masking marker present: {summary}"
        );
        assert!(
            !summary.contains('…'),
            "clamp ellipsis introduced: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_preserves_webhook_url() {
        // /api/webhooks/ を含む URL がそのまま残ること（バイト一致）。
        let url =
            "https://discord.com/api/webhooks/123456789012345678/AbCdEf-XXXXXXXXXXXXXXXXXXXXXXXX";
        let args = serde_json::json!({ "command": format!("curl {url}") });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains(url), "webhook url stripped: {summary}");
        assert!(
            !summary.contains("[redacted]"),
            "url masking present: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_execute_shell_includes_command_and_args() {
        // execute_shell の実引数（command + args 配列）が両方描画されること。
        let args = serde_json::json!({
            "command": "echo",
            "args": ["hello", "webhook-args-test"]
        });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains("echo"), "command missing: {summary}");
        assert!(summary.contains("hello"), "first arg missing: {summary}");
        assert!(
            summary.contains("webhook-args-test"),
            "second arg missing: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_execute_shell_marks_stdin_without_leaking() {
        // stdin は本文を出さず、存在とバイト数のみ示す。
        let args = serde_json::json!({
            "command": "cat",
            "stdin": "secret-stdin-body"
        });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains("cat"), "command missing: {summary}");
        assert!(summary.contains("stdin"), "stdin marker missing: {summary}");
        assert!(
            !summary.contains("secret-stdin-body"),
            "stdin body leaked: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_empty_is_none() {
        assert!(summarize_tool_args("read_file", &serde_json::json!({})).is_none());
        assert!(summarize_tool_args("read_file", &serde_json::Value::Null).is_none());
    }

    // ---- L1: disabled/invalid activity row drops events (no silent fallback) ----

    #[test]
    fn test_webhook_tool_event_sink_disabled_activity_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", false);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({});
        opencrab_actions::ToolEventSink::on_event(&sink, &started_event(&args));
        assert!(rx.try_recv().is_err(), "disabled activity -> nothing sent");
    }

    #[test]
    fn test_webhook_tool_event_sink_invalid_activity_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        // invalid (non-discord) url -> WebhookResolution::Error, must drop, no fallback.
        insert_activity_row(&conn, "https://evil.example.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({});
        opencrab_actions::ToolEventSink::on_event(&sink, &started_event(&args));
        assert!(
            rx.try_recv().is_err(),
            "invalid activity url -> nothing sent"
        );
    }

    // ---- L2: shared delivery path preserves lifecycle/tool_call ordering ----

    #[test]
    fn test_shared_worker_channel_preserves_order() {
        // 単一の共有 tx を使うと、先に送った lifecycle batch のあとに tool_call event が
        // 続き、FIFO 順序が保たれる（別 worker だと順序保証が崩れる）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();

        // lifecycle 相当の batch を共有 tx へ先に送る。
        tx.send(DeliveryBatch {
            url: "https://discord.com/api/webhooks/1/tok".to_string(),
            messages: vec!["lifecycle: started".to_string()],
        })
        .unwrap();

        // 同じ tx を使う sink から tool_call event を送る。
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({ "exit_code": 0, "stdout": "ok", "truncated": false });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "t",
            duration_ms: Some(1),
            args: &args,
            result: Some(&result),
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);

        // 受信順: lifecycle が先、tool_call が後。
        let first = rx.try_recv().expect("lifecycle batch");
        assert!(first.messages[0].contains("lifecycle: started"));
        let second = rx.try_recv().expect("tool_call batch");
        assert!(second.messages[0].contains("tool_call_completed"));
    }

    #[test]
    fn test_activity_diagnostic_batch_for_invalid_explicit_webhook_url() {
        // 非空の不正 explicit url は resolution Error を生み、その診断が activity default
        // へ redacted で配送されることを担保する。空 url はもはや Error にならない
        // （default へフォールバックする）ため、ここでは非空の不正 url を使う。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let args = serde_json::json!({
            "task": "do it",
            "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" }
        });
        let batch = build_activity_diagnostic_batch(
            &db,
            "a1",
            "spawn_subtask",
            "webhook_resolution_error",
            "spawn_subtask webhook resolution failed before execution: invalid_webhook_url: url must start with https:// (source: explicit)",
            &args,
        )
        .expect("diagnostic should route to activity default");
        assert_eq!(batch.url, "https://discord.com/api/webhooks/1/tok");
        let msg = &batch.messages[0];
        assert!(msg.contains("webhook_resolution_error"));
        assert!(msg.contains("invalid_webhook_url"));
        assert!(msg.contains("source: explicit"));
        assert!(!msg.contains("https://discord.com/api/webhooks/1/tok"));
    }

    // ---- depth0/main executor sink wiring (factory) ----

    /// activity 行が無いエージェントでは factory は None を返す（worker も起動しない）。
    #[tokio::test]
    async fn test_spawn_activity_sink_none_without_activity_row() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(sink.is_none(), "no activity row -> no sink");
    }

    /// activity 行があれば factory は Some を返し、その sink は depth0 イベントを
    /// activity webhook へ整形して配送する（covered 経路ゆえ unredacted で配送する）。
    #[tokio::test]
    async fn test_spawn_activity_sink_some_with_activity_row_and_delivers() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(sink.is_some(), "activity row -> sink present");

        // depth0 のツールイベントを流すと配送される（worker が実際に送ろうとするが、
        // ダミー URL なのでネットワークは best-effort で失敗する。ここでは on_event が
        // パニックせず整形できることを確認する）。
        let sink = sink.unwrap();
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
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&result),
            error: None,
        };
        sink.on_event(&ev);
    }

    fn insert_global_activity(conn: &rusqlite::Connection) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "global".to_string(),
            agent_id: "*".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: "https://discord.com/api/webhooks/9/glob".to_string(),
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

    /// global(`*`) のみの activity デフォルトでも factory は Some を返す
    /// （list_agent_webhook_config が agent_id='*' を含むため）。depth0 イベントが
    /// global 宛先へ stream され得ることを担保する。
    #[tokio::test]
    async fn test_spawn_activity_sink_some_with_global_only_activity_row() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_global_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        // agent "a1" 固有の行は無いが、global 行があるので Some。
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(
            sink.is_some(),
            "global-only activity default -> sink present"
        );
    }
}
