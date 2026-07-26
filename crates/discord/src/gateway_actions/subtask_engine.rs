//! サブタスクエンジン操作 (spawn_subtask, cancel_subtask, report_progress)

use std::sync::Arc;

use chrono::Utc;
use opencrab_gateway::{GatewayActionResult, GatewayCallContext};
use serde_json::json;
use uuid::Uuid;

use super::subtask_webhook::reject;
use super::webhook::{self, DeliveryBatch, WebhookResolution};
use super::{ArcLlmClient, DiscordGatewayActions};
use crate::message_loop::{parse_discord_session, LoopEvent};
use opencrab_actions::subtask::{
    settle_completed, SettleContext, SettleKind, SpawnedSubtask, SubtaskCompletionSink,
    SubtaskSettled,
};
use opencrab_actions::subtask_notify::SubtaskRunInfo;
use opencrab_actions::SubEngineGatewayActions;

/// `SubtaskCompletionSink` の Discord 実装（RFC #152 S1）。
///
/// 旧 `send_subtask_completed_event`（LoopEvent 直依存）を置換する。runtime
/// （actions 側 `settle_completed` / progress debounce）は `Arc<dyn
/// SubtaskCompletionSink>` としてこれを呼ぶだけで、`LoopEvent` を知らない。
/// `parse_discord_session` / `LoopEvent` は Discord に閉じたままここに残す。
///
/// parent_session_id から routing 情報を復元して `LoopEvent::SubtaskCompleted` を送る
/// （#39）。session_id（`discord-{agent}-{guild}-{channel}`）から導出できるため、
/// クロージャの登録は不要。event_tx 未設定（イベントループの無い構築、例: 一発呼びの
/// API 経路）や Discord 形式でない session は、旧実装で未登録だった場合と同様に発火
/// しない（debug のみ）。
///
/// **web / Nostr sink との意図的な差分**: あちらは `kind != SettleKind::Completed` を
/// 捨てるが、Discord は `Progress` も送る。`report_progress` のデバウンス発火が
/// この sink を通ってメインエンジンを呼び直す「進捗実況」機能で、main の
/// `send_subtask_completed_event(..., "progress")` から続く既存挙動だから
/// （ガードを足すと機能が黙って消える）。`Cancelled` は別メソッド
/// （`on_subtask_cancelled` の既定実装 = 何もしない）なのでここには来ない。
/// この差分は `discord_sink_forwards_progress_unlike_web_and_nostr` で固定している。
pub(crate) struct DiscordCompletionSink {
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<LoopEvent>>,
}

impl SubtaskCompletionSink for DiscordCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        let Some(tx) = &self.event_tx else {
            tracing::debug!(
                session_id = %ev.session_id,
                "subtask completion: event_tx not configured, skipping main-engine notification"
            );
            return;
        };
        let Some((guild_id, channel_id)) = parse_discord_session(&ev.session_id) else {
            // 非 Discord の親セッション（heartbeat-* / subtask-* のネスト等）は正常系。
            // 旧レジストリ実装でも未登録で発火しなかったため、debug に留める。
            tracing::debug!(
                session_id = %ev.session_id,
                "subtask completion: parent session is not a discord session, skipping main-engine notification"
            );
            return;
        };
        let is_dm = guild_id.is_empty();
        let _ = tx.send(LoopEvent::SubtaskCompleted {
            session_id: ev.session_id,
            agent_id: ev.agent_id,
            subtask_id: ev.subtask_id,
            // 本文は運ばない。完了本文は DB（session_logs）へ永続化済みで、再注入は
            // `build_conversation_string` が DB から読み直す（`process_subtask_completed`
            // の引数は `_result` = 未使用。RFC §1.3）。
            result: String::new(),
            exit_reason: ev.exit_reason,
            channel_id,
            channel_id_str: channel_id.to_string(),
            guild_id,
            is_dm,
        });
    }
}

/// Map an `LlmCallLog` from a sub-engine to an `llm_logs` row (#148).
///
/// The sub-engine (spawned by `execute_spawn_subtask`) was previously unwired
/// for llm logging — only depth0 was recorded via the server's
/// `set_llm_log_callback` — so subtask inference and its tokens were lost. This
/// mirrors the depth0 logic (crates/server/src/process.rs) column-for-column,
/// tagging rows with `session_id = subtask-{id}`. `trigger_message_id` is `None`
/// because a subtask has no originating Discord message. Extracted from the
/// callback closure so the mapping can be unit-tested without running an engine.
fn build_subtask_llm_log_row(
    agent_id: &str,
    session_id: &str,
    log: &opencrab_core::LlmCallLog,
) -> opencrab_db::queries::LlmLogRow {
    let (prompt_tokens, completion_tokens, total_tokens) = log
        .response
        .as_ref()
        .map(|r| &r.usage)
        .map(|u| {
            (
                Some(u.prompt_tokens as i64),
                Some(u.completion_tokens as i64),
                Some(u.total_tokens as i64),
            )
        })
        .unwrap_or((None, None, None));

    let cache_read_tokens = log
        .response
        .as_ref()
        .map(|r| &r.usage)
        .map(|u| u.cache_read_input_tokens as i64);
    let cache_creation_tokens = log
        .response
        .as_ref()
        .map(|r| &r.usage)
        .map(|u| u.cache_creation_input_tokens as i64);

    let response_str = log
        .response
        .as_ref()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .unwrap_or_default();

    opencrab_db::queries::LlmLogRow {
        id: Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: Some(session_id.to_string()),
        model: Some(log.request.model.clone()),
        prompt: serde_json::to_string(&log.request).unwrap_or_default(),
        response: response_str,
        tool_calls: log
            .response
            .as_ref()
            .and_then(|r| r.first_message())
            .and_then(|m| m.tool_calls.as_ref())
            .filter(|tc| !tc.is_empty())
            .and_then(|tc| serde_json::to_string(tc).ok()),
        latency_ms: Some(log.latency_ms),
        prompt_tokens,
        completion_tokens,
        total_tokens,
        error_code: log.error_str.as_ref().map(|_| "error".to_string()),
        error_body: log.error_str.clone(),
        requested_at: Some(log.requested_at.clone()),
        trigger_message_id: None,
        is_bot_iteration: log.is_bot_iteration,
        cache_read_tokens,
        cache_creation_tokens,
        created_at: chrono::Utc::now().to_rfc3339(),
    }
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

        let label = args["label"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| task.chars().take(50).collect::<String>());

        // lifecycle 通知は抽象境界（`SubtaskLifecycleNotifier`）越しに扱う（#175 S3）。
        // 宛先の解決・配送ワーカーの起動・整形はすべて実装側（Discord なら
        // `DiscordWebhookNotifier`）に閉じており、ここは「起きた事実」を渡すだけ。
        // 解決に失敗したら spawn しない（fail-closed）。raw url はどこにも出さない。
        let notify = match self.lifecycle_notifier().begin_run(&SubtaskRunInfo {
            agent_id: &agent_id,
            subtask_id: &subtask_id,
            sub_session_id: &sub_session_id,
            parent_session_id: &parent_session_id,
            label: &label,
            tool_args: args,
        }) {
            Ok(session) => session,
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    error: Some(format!("{}: {}", e.code, e.message)),
                    data: Some(json!({
                        "webhook_source": e.source,
                        "webhook_status": "error",
                        "webhook_error": e.message,
                    })),
                };
            }
        };
        let notifier = notify.notifier;
        let webhook_source_str = notify.target.source;
        let webhook_status = notify.target.status;
        let webhook_redacted_url = notify.target.redacted_url;

        // 開始を通知する。
        notifier.on_started(&task);

        // 通知実装が実況用の sink を持っていれば sub-engine の executor に挿す。
        let tool_event_sink = notifier.tool_event_sink();

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
                opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
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
        let ws_path = match self.agent_workspace_root(&agent_id) {
            Ok(root) => root.join(&agent_id),
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("spawn_subtask: workspace error: {e}")),
                };
            }
        };
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
            // 許可リストラッパ経由で gateway を接続する（#63 / RFC #152 S2）。これが無いと
            // system prompt が指示する report_progress が "Unknown action" で失敗する。
            //
            // inner は「自分を包む合成 gateway」（`SystemGatewayActions` = server ツール +
            // transport の union）を優先する。ctx.root_gateway は `BridgedExecutor` が
            // 注入したハンドル。これにより sub-engine から server ツール
            // （`nostr_generate_key` 等、許可リスト内のもの）へ到達できる。
            // 未注入（後方互換の経路）なら従来通り transport gateway 単体（`self`）を包む。
            let inner_gateway: Arc<dyn opencrab_gateway::GatewayActions> = match &ctx.root_gateway {
                Some(root) => root.clone(),
                None => Arc::new(self.clone()),
            };
            let sub_gateway: Arc<dyn opencrab_gateway::GatewayActions> =
                Arc::new(SubEngineGatewayActions::new(inner_gateway));
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

        // sub-engine のツール呼び出し/結果を進捗として実況する。購読していない
        // （`wants_progress()` が false）ならフック自体を挿さず、要約の計算も省く。
        if notifier.wants_progress() {
            let progress_notifier = notifier.clone();
            sub_engine.set_on_tool_call(move |assistant_content, tool_calls_json| {
                let detail = summarize_tool_calls(&assistant_content, &tool_calls_json);
                progress_notifier.on_progress(&detail);
            });

            let progress_notifier = notifier.clone();
            sub_engine.set_on_tool_result(
                move |_tool_call_id, tool_name, result_json, is_error| {
                    let status = if is_error { "failed" } else { "completed" };
                    let preview: String = result_json.chars().take(500).collect();
                    let detail = format!("tool `{tool_name}` {status}\n{preview}");
                    progress_notifier.on_progress(&detail);
                },
            );
        }

        // sub-engine の LLM 呼び出しも llm_logs に記録する（#148: depth0 メインは
        // server 側 set_llm_log_callback で記録するが、sub-engine は未配線でトークン計上が
        // 漏れていた）。opencrab-server の private 関数は使えないため、depth0 実装
        // （crates/server/src/process.rs:853-924）と列名/型を一致させて同等ロジックを
        // インラインする。trigger_message_id は subtask には無いので None。
        {
            let log_db = self.db.clone();
            let log_agent_id = agent_id.clone();
            let log_session_id = sub_session_id.clone();
            sub_engine.set_log_callback(move |log: &opencrab_core::LlmCallLog| {
                let log_row = build_subtask_llm_log_row(&log_agent_id, &log_session_id, log);
                if let Ok(conn) = log_db.lock() {
                    if let Err(e) = opencrab_db::queries::insert_llm_log(&conn, &log_row) {
                        tracing::error!("Failed to insert llm_log (subtask): {e}");
                    }
                }
            });
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
        // 完了通知は sink 経由（LoopEvent 直依存を置換）。event_tx から Discord 実装を
        // 組み立て、runtime 中核 `settle_completed` へ `&dyn` として渡す。
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(DiscordCompletionSink {
            event_tx: self.event_tx.clone(),
        });
        let subtask_registry_clone = self.subtask_registry.clone();
        let subtask_notifiers_clone = self.subtask_notifiers.clone();
        let progress_debounce_clone = self.progress_debounce.clone();
        let default_model_clone = effective_model.clone();
        let notifier_task = notifier.clone();

        // 開始ゲート: 親がレジストリへ insert し終えるまでタスク本体を走らせない。
        // これが無いと、即座に失敗するサブタスクが親の insert より先に remove を実行し、
        // その後 insert が着地して「running のまま」のエントリがリークする。
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        // 停止/決着の排他ラッチ。sub-engine が完走してから settle が DB へ着地する
        // までの窓で cancel が入っても、完了ログと sink 発火は行われない
        // （= 止めたのに返信が届くのを防ぐ）。
        let lifecycle = opencrab_actions::SubtaskLifecycle::new();
        let lifecycle_task = lifecycle.clone();

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

            // --- gateway 固有の後始末（DB 永続化 / sink 発火の前に済ませる。RFC #152
            //     S1: これらは DB 永続化とも sink 発火とも順序依存が無い＝webhook は
            //     非同期配送・別マップ。載せ替えても観測挙動は不変）。---

            // 終了（正常 / 異常 / タイムアウト）を通知する。表示状態への写像と購読
            // フィルタは通知実装側の責務。
            notifier_task.on_finished(
                &exit_reason,
                started_instant.elapsed().as_millis() as u64,
                &result_text,
            );

            // 保留中の progress デバウンスを無効化する。エントリが消えると、まだ
            // sleep 中のデバウンスタスクは is_latest=false 扱いになり発火しない。
            // これが無いと、終了イベントの後に遅延 progress（0〜3秒窓）が届いて
            // 完了返信の直後に余計な推論・重複返信が走ることがある（#86 レビュー指摘。
            // 同一親セッションの兄弟サブタスクの保留 progress も巻き添えで消えるが、
            // progress は advisory であり次の report_progress で再アームされる）。
            progress_debounce_clone.remove(&parent_session_clone);

            // 通知口を registry と対で除去する。
            subtask_notifiers_clone.remove(&subtask_id_clone);

            // --- 中核（gateway 非依存）: DB へ subtask_completed を永続化 → registry
            //     除去 → sink 発火。順序契約（DB 記録 → 通知）は settle_completed が
            //     1 箇所で保証する（RFC §6 受け入れ基準）。sink は本文を運ばない。---
            settle_completed(
                &subtask_registry_clone,
                &db_clone,
                sink.as_ref(),
                SettleContext {
                    parent_session_id: parent_session_clone,
                    agent_id: agent_id_clone,
                    subtask_id: subtask_id_clone,
                    sub_session_id: sub_session_id_clone,
                    exit_reason,
                    lifecycle: lifecycle_task,
                },
                &result_text,
            );
        });

        let abort_handle = join_handle.abort_handle();
        self.subtask_registry.insert(
            subtask_id.clone(),
            SpawnedSubtask {
                abort_handle,
                session_id: sub_session_id.clone(),
                parent_session_id: parent_session_id.clone(),
                agent_id: agent_id.clone(),
                label: label.clone(),
                started_at: started_instant,
                // Discord は返信先を parent_session_id から parse_discord_session で
                // 復元する（DiscordCompletionSink）ため、reply_target は未使用。
                reply_target: None,
                lifecycle,
            },
        );
        // 通知口（gateway 固有の実装を隠した trait オブジェクト）は registry と対の
        // 随伴マップへ分離する（RFC §1.5）。cancel / report_progress はここから引く。
        self.subtask_notifiers.insert(subtask_id.clone(), notifier);

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
        // 述語内で停止を主張（`claim_cancel`）することで、abort が効かない窓
        // （sub-engine が完走して settle_completed へ入った後）でも「cancel 成功 ＋
        // 完了ログ着地 ＋ sink 発火」の二重決着にならない。既に決着済みなら claim に
        // 失敗し、エントリは残したまま not found を返す（通常完了として通知される）。
        match self.subtask_registry.remove_if(&subtask_id, |_, subtask| {
            authorized(subtask) && subtask.lifecycle.claim_cancel()
        }) {
            Some((_, subtask)) => {
                subtask.abort_handle.abort();

                // 中断を通知する。アボートで spawned closure は中断されるため終了通知は
                // 来ない → ここが唯一の終端。通知口は registry と対の随伴マップから引く
                // （RFC §1.5）。
                if let Some((_, notifier)) = self.subtask_notifiers.remove(&subtask_id) {
                    notifier.on_cancelled(subtask.started_at.elapsed().as_millis() as u64);
                }

                // Write subtask_cancelled to parent session log.
                let parent_session_id = subtask.parent_session_id.clone();
                if !parent_session_id.is_empty() {
                    if let Ok(conn) = self.db.lock() {
                        // 停止対象の説明は sub-session の theme を第一候補にする
                        // （明示的な `spawn_subtask` はここに人間可読なテーマを持つ）。
                        //
                        // ただし自動 dispatch で背景実行に回った subtask は sub-session
                        // の行を作らないため theme を引けず、そのままだと親ログが
                        // `subtask '' was cancelled` になって「どのツールを止めたのか」
                        // が分からない（#176）。theme が引けない/空のときは registry が
                        // 保持する label（例: `execute_shell(...)`）へフォールバックする。
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
                                .filter(|desc| !desc.is_empty())
                                .unwrap_or_else(|| subtask.label.clone());
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
                        opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
                    }
                }

                GatewayActionResult {
                    success: true,
                    data: Some(json!({"cancelled": true, "subtask_id": subtask_id})),
                    error: None,
                }
            }
            None => {
                // remove_if の None は「不在」「権限なし」「既に決着済み（claim 失敗）」。
                // 所有権フィールドは不変なので、残っていて決着中でなければ権限なし。
                let already_settling = self
                    .subtask_registry
                    .get(&subtask_id)
                    .map(|e| e.lifecycle.is_settling())
                    .unwrap_or(false);
                if self.subtask_registry.contains_key(&subtask_id) && !already_settling {
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
                opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
            }
        }

        if let Some((resolved_subtask_id, _)) = &subtask_entry {
            // 進捗を通知する。通知口は registry と対の随伴マップから引く（RFC §1.5）。
            if let Some(notifier) = self.subtask_notifiers.get(resolved_subtask_id) {
                notifier.on_progress(&message);
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
        // 完了通知と同じ sink 経由で進捗トリガを送る（LoopEvent 直依存を置換）。
        // 進捗本文は上で親セッションログ（DB）へ永続化済みのため、sink には運ばない。
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(DiscordCompletionSink {
            event_tx: self.event_tx.clone(),
        });
        let progress_debounce_clone = self.progress_debounce.clone();
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let agent_id_clone = agent_id.clone();
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
            sink.on_subtask_settled(SubtaskSettled {
                session_id: parent_session_clone,
                agent_id: agent_id_clone,
                subtask_id: subtask_id_clone,
                exit_reason: "progress".to_string(),
                kind: SettleKind::Progress,
                // Discord は parent_session_id から返信先を復元するため不要（#167）。
                reply_target: None,
            });
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

pub(super) fn emit_activity_diagnostic(
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
pub(super) struct WebhookToolEventSink {
    db: opencrab_db::Db,
    agent_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    max_chars: usize,
    counter: AtomicUsize,
    cap: usize,
}

impl WebhookToolEventSink {
    pub(super) fn new(
        db: opencrab_db::Db,
        agent_id: String,
        tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
        max_chars: usize,
        cap: usize,
    ) -> Self {
        Self {
            db,
            agent_id,
            tx,
            max_chars,
            counter: AtomicUsize::new(0),
            cap,
        }
    }
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

    // ---- #175 S3: lifecycle 通知の抽象境界（trait 経由の配線） ----
    //
    // 通知の中身（整形）は Discord 実装側のテストで固定している。ここで固定するのは
    // 「3 つのツール本体が trait を実際に呼ぶ」という配線そのもので、呼び出しを 1 つ
    // 落とせば対応するテストが落ちる。

    /// テスト用の通知口。呼ばれたイベントを順に記録する。
    #[derive(Default)]
    struct RecordingNotifier {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingNotifier {
        fn record(&self, ev: String) {
            self.events.lock().unwrap().push(ev);
        }
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    impl opencrab_actions::subtask_notify::SubtaskRunNotifier for RecordingNotifier {
        fn on_started(&self, task: &str) {
            self.record(format!("started:{task}"));
        }
        fn on_progress(&self, detail: &str) {
            self.record(format!("progress:{detail}"));
        }
        fn on_finished(&self, exit_reason: &str, _duration_ms: u64, result_text: &str) {
            self.record(format!("finished:{exit_reason}:{result_text}"));
        }
        fn on_cancelled(&self, _duration_ms: u64) {
            self.record("cancelled".to_string());
        }
        fn wants_progress(&self) -> bool {
            true
        }
    }

    /// `RecordingNotifier` を返すファクトリ。
    struct RecordingNotifierFactory {
        notifier: Arc<RecordingNotifier>,
    }

    impl opencrab_actions::subtask_notify::SubtaskLifecycleNotifier for RecordingNotifierFactory {
        fn begin_run(
            &self,
            run: &opencrab_actions::subtask_notify::SubtaskRunInfo<'_>,
        ) -> Result<
            opencrab_actions::subtask_notify::SubtaskNotifySession,
            opencrab_actions::subtask_notify::NotifyTargetError,
        > {
            self.notifier.record(format!("begin:{}", run.label));
            Ok(opencrab_actions::subtask_notify::SubtaskNotifySession {
                notifier: self.notifier.clone(),
                target: opencrab_actions::subtask_notify::NotifyTarget::none(),
            })
        }
    }

    /// LLM を呼ばずに 1 往復で完了する stub（sub-engine を最後まで走らせるため）。
    struct StubLlmClient;

    #[async_trait::async_trait]
    impl opencrab_core::LlmClient for StubLlmClient {
        async fn chat(
            &self,
            _request: opencrab_core::ChatRequest,
        ) -> anyhow::Result<opencrab_core::ChatResponse> {
            Ok(opencrab_core::ChatResponse::text("sub-engine done"))
        }
    }

    /// テスト用の gateway actions（Discord HTTP は呼ばない）。
    fn wiring_actions(
        llm: Option<Arc<dyn opencrab_core::LlmClient>>,
        workspace_base: &str,
    ) -> DiscordGatewayActions {
        let db = opencrab_db::Db::memory().unwrap();
        DiscordGatewayActions::new(
            Arc::new(serenity::http::Http::new("dummy-token")),
            db,
            Arc::new(std::sync::RwLock::new(
                opencrab_actions::tools::ToolsConfig::default(),
            )),
            llm,
            "test-model".to_string(),
            workspace_base.to_string(),
            Arc::new(dashmap::DashMap::new()),
            None,
        )
    }

    fn wiring_ctx(caller: opencrab_gateway::GatewayCaller, session_id: &str) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent").with_session_id(session_id)
    }

    /// registry に走行中サブタスクを 1 件登録する（abort されない pending タスク）。
    fn insert_running_subtask(
        actions: &DiscordGatewayActions,
        subtask_id: &str,
        session_id: &str,
        parent_session_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let handle = tokio::spawn(std::future::pending::<()>());
        actions.subtask_registry.insert(
            subtask_id.to_string(),
            SpawnedSubtask {
                abort_handle: handle.abort_handle(),
                session_id: session_id.to_string(),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "test-agent".to_string(),
                label: "job".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            },
        );
        handle
    }

    /// `report_progress` は随伴マップの通知口へ進捗を渡す。
    #[tokio::test]
    async fn report_progress_notifies_through_trait() {
        let actions = wiring_actions(None, "/tmp");
        let _h = insert_running_subtask(
            &actions,
            "st-1",
            "subtask-st-1",
            "discord-test-agent-111-222",
        );
        let recorder = Arc::new(RecordingNotifier::default());
        actions
            .subtask_notifiers
            .insert("st-1".to_string(), recorder.clone());

        let res = actions
            .execute_report_progress(
                &json!({"message": "halfway there"}),
                &wiring_ctx(opencrab_gateway::GatewayCaller::Agent, "subtask-st-1"),
            )
            .await;
        assert!(res.success, "report_progress: {:?}", res.error);
        assert_eq!(recorder.events(), vec!["progress:halfway there"]);
    }

    /// `cancel_subtask` は通知口へ中断を伝え、随伴マップから外す。
    #[tokio::test]
    async fn cancel_subtask_notifies_through_trait() {
        let actions = wiring_actions(None, "/tmp");
        let _h = insert_running_subtask(
            &actions,
            "st-1",
            "subtask-st-1",
            "discord-test-agent-111-222",
        );
        let recorder = Arc::new(RecordingNotifier::default());
        actions
            .subtask_notifiers
            .insert("st-1".to_string(), recorder.clone());

        let res = actions.execute_cancel_subtask(
            &json!({"subtask_id": "st-1"}),
            &wiring_ctx(
                opencrab_gateway::GatewayCaller::Owner,
                "discord-test-agent-111-222",
            ),
        );
        assert!(res.success, "cancel_subtask: {:?}", res.error);
        assert_eq!(recorder.events(), vec!["cancelled"]);
        assert!(
            !actions.subtask_notifiers.contains_key("st-1"),
            "通知口は registry と対で除去する"
        );
    }

    /// `spawn_subtask` は走行の開始と終了を trait 経由で通知し、決着時に通知口を外す。
    #[tokio::test]
    async fn spawn_subtask_notifies_started_and_finished_through_trait() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Arc::new(RecordingNotifier::default());
        let actions = wiring_actions(Some(Arc::new(StubLlmClient)), tmp.path().to_str().unwrap())
            .with_lifecycle_notifier(Arc::new(RecordingNotifierFactory {
                notifier: recorder.clone(),
            }));

        let res = actions
            .execute_spawn_subtask(
                &json!({"task": "do the thing", "label": "job"}),
                &wiring_ctx(
                    opencrab_gateway::GatewayCaller::Agent,
                    "discord-test-agent-111-222",
                ),
            )
            .await;
        assert!(res.success, "spawn_subtask: {:?}", res.error);
        let subtask_id = res.data.as_ref().unwrap()["subtask_id"]
            .as_str()
            .unwrap()
            .to_string();

        // 開始は同期的に通知される。
        assert_eq!(
            recorder.events(),
            vec!["begin:job".to_string(), "started:do the thing".to_string()]
        );

        // 終了は spawn したタスクから通知される。
        for _ in 0..200 {
            if recorder.events().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let events = recorder.events();
        assert_eq!(
            events.get(2).map(|s| s.as_str()),
            Some("finished:completed:sub-engine done"),
            "終了通知が届かない: {events:?}"
        );
        assert!(
            !actions.subtask_notifiers.contains_key(&subtask_id),
            "決着後は通知口を外す"
        );
    }

    /// 通知先を持たない構成（`NoopLifecycleNotifier`）でも subtask 生成は成立する
    /// （= Discord 抜きでも通知の依存で詰まらない。S4 の前提）。
    #[tokio::test]
    async fn spawn_subtask_works_with_noop_notifier() {
        let tmp = tempfile::tempdir().unwrap();
        let actions = wiring_actions(Some(Arc::new(StubLlmClient)), tmp.path().to_str().unwrap())
            .with_lifecycle_notifier(Arc::new(opencrab_actions::NoopLifecycleNotifier));

        let res = actions
            .execute_spawn_subtask(
                &json!({"task": "do the thing"}),
                &wiring_ctx(
                    opencrab_gateway::GatewayCaller::Agent,
                    "discord-test-agent-111-222",
                ),
            )
            .await;
        assert!(res.success, "spawn_subtask: {:?}", res.error);
        let data = res.data.unwrap();
        assert_eq!(data["status"], "spawned");
        assert_eq!(data["webhook_status"], "none");
        assert!(data["webhook_source"].is_null());
        assert!(data["webhook_redacted_url"].is_null());
    }

    // ---- RFC #152 S1: DiscordCompletionSink（完了の再注入経路） ----
    //
    // この sink は「dispatch した全ツールの結果を親会話へ戻す」唯一の口なので、
    // 空実装にしても他テストが緑のままだと退行を検知できない（#165 レビュー P1。
    // web sink に対する同種の指摘と同じ）。実 mpsc を張って、送出の有無と
    // routing 復元内容をここで直接固定する。

    /// テスト用: 実チャネルを張った sink と受信側を作る。
    fn sink_with_channel() -> (
        DiscordCompletionSink,
        tokio::sync::mpsc::UnboundedReceiver<LoopEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (DiscordCompletionSink { event_tx: Some(tx) }, rx)
    }

    fn settled(session_id: &str, kind: SettleKind, exit_reason: &str) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-x".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: exit_reason.to_string(),
            kind,
            reply_target: None,
        }
    }

    /// 完了 → `LoopEvent::SubtaskCompleted` がちょうど 1 本流れ、guild/channel が
    /// parent_session_id から復元される（本文は運ばない = `result` は空）。
    #[test]
    fn discord_sink_emits_loop_event_on_completion() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_settled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Completed,
            "completed",
        ));

        match rx.try_recv().expect("完了は LoopEvent を 1 本送る") {
            LoopEvent::SubtaskCompleted {
                session_id,
                agent_id,
                subtask_id,
                result,
                exit_reason,
                channel_id,
                channel_id_str,
                guild_id,
                is_dm,
            } => {
                assert_eq!(session_id, "discord-agent-x-111222333-444555666");
                assert_eq!(agent_id, "agent-x");
                assert_eq!(subtask_id, "st-1");
                // 本文は DB（session_logs）から読み直す契約（RFC §1.3）。
                assert_eq!(result, "");
                assert_eq!(exit_reason, "completed");
                assert_eq!(channel_id, 444_555_666);
                assert_eq!(channel_id_str, "444555666");
                assert_eq!(guild_id, "111222333");
                assert!(!is_dm);
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
        assert!(rx.try_recv().is_err(), "余分なイベントを送ってはならない");
    }

    /// DM（guild_id 空）の親セッションでも復元でき、`is_dm` が立つ。
    #[test]
    fn discord_sink_restores_dm_routing() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_settled(settled(
            "discord-agent-x--444555666",
            SettleKind::Completed,
            "timeout",
        ));

        match rx.try_recv().expect("DM でも LoopEvent を送る") {
            LoopEvent::SubtaskCompleted {
                guild_id,
                channel_id,
                is_dm,
                exit_reason,
                ..
            } => {
                assert_eq!(guild_id, "");
                assert_eq!(channel_id, 444_555_666);
                assert!(is_dm, "guild_id が空なら DM 扱い");
                // exit_reason は完了理由をそのまま運ぶ（completed 以外も再注入する）。
                assert_eq!(exit_reason, "timeout");
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
        assert!(rx.try_recv().is_err());
    }

    /// 非 Discord の親セッション（web / heartbeat / nostr / ネストした subtask）は
    /// 正常系としてスキップする。各 gateway の sink が自分のセッションだけを拾う。
    #[test]
    fn discord_sink_skips_non_discord_sessions() {
        for session_id in [
            "web-agent-x-conv-1",
            "heartbeat-agent-x",
            "nostr-agent-x-npub1abc",
            "subtask-11111111-2222-3333-4444-555555555555",
            "agent-msg-agent-x-user-1",
            "",
        ] {
            let (sink, mut rx) = sink_with_channel();
            sink.on_subtask_settled(settled(session_id, SettleKind::Completed, "completed"));
            assert!(
                rx.try_recv().is_err(),
                "非 Discord セッション '{session_id}' で LoopEvent を送ってはならない"
            );
        }
    }

    /// Discord 形式に見えて壊れている session_id（channel が数値でない等）も送らない。
    #[test]
    fn discord_sink_skips_malformed_discord_sessions() {
        for session_id in [
            "discord-agent-x-111-notanumber",
            "discord-agent-x-notanumber-444",
            "discord--111-444",
            "discord-agent-x",
        ] {
            let (sink, mut rx) = sink_with_channel();
            sink.on_subtask_settled(settled(session_id, SettleKind::Completed, "completed"));
            assert!(
                rx.try_recv().is_err(),
                "壊れた session_id '{session_id}' で LoopEvent を送ってはならない"
            );
        }
    }

    /// **意図的な差分**: Discord は `SettleKind::Progress` でも LoopEvent を送る
    /// （web / Nostr の sink は Completed 以外を捨てる）。
    ///
    /// `report_progress` のデバウンス発火はこの sink を通ってメインエンジンを
    /// 呼び直す実況機能で、main の `send_subtask_completed_event(..., "progress")`
    /// から続く既存挙動。ここで捨てると進捗実況が黙って消えるため、
    /// web / Nostr と同じ `kind != Completed` ガードは**入れない**。
    /// 差分を退行ではなく仕様として固定するためのテスト。
    #[test]
    fn discord_sink_forwards_progress_unlike_web_and_nostr() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_settled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Progress,
            "progress",
        ));

        match rx.try_recv().expect("進捗もメインエンジンへ再注入する") {
            LoopEvent::SubtaskCompleted { exit_reason, .. } => {
                assert_eq!(exit_reason, "progress");
            }
            _ => panic!("SubtaskCompleted 以外のイベントが流れた"),
        }
    }

    /// `on_subtask_cancelled` は既定実装（debug ログのみ）のまま = 停止では
    /// 再注入しない（止めたのに返信が届くのを防ぐ）。
    #[test]
    fn discord_sink_does_not_reinject_on_cancel() {
        let (sink, mut rx) = sink_with_channel();
        sink.on_subtask_cancelled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Cancelled,
            "cancelled",
        ));
        assert!(
            rx.try_recv().is_err(),
            "cancel で LoopEvent を送ってはならない"
        );
    }

    /// event_tx 未設定（イベントループの無い構築）は no-op で panic しない。
    #[test]
    fn discord_sink_without_event_tx_is_noop() {
        let sink = DiscordCompletionSink { event_tx: None };
        sink.on_subtask_settled(settled(
            "discord-agent-x-111222333-444555666",
            SettleKind::Completed,
            "completed",
        ));
    }

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

    // ---- #148: subtask sub-engine LLM logging ----

    /// A successful sub-engine call maps to an llm_logs row tagged with the
    /// subtask session id, with tokens accounted and no trigger message.
    #[test]
    fn build_subtask_llm_log_row_accounts_tokens_and_tags_session() {
        let mut resp = opencrab_core::ChatResponse::text("done");
        resp.model = "gpt-test".to_string();
        resp.usage.prompt_tokens = 100;
        resp.usage.completion_tokens = 20;
        resp.usage.total_tokens = 120;
        resp.usage.cache_read_input_tokens = 30;
        resp.usage.cache_creation_input_tokens = 5;
        let log = opencrab_core::LlmCallLog {
            request: opencrab_core::ChatRequest::new("gpt-test", vec![]),
            response: Some(resp),
            error_str: None,
            latency_ms: 42,
            requested_at: "2026-07-25T00:00:00Z".to_string(),
            is_bot_iteration: true,
        };
        let row = build_subtask_llm_log_row("agent-x", "subtask-abc", &log);
        assert_eq!(row.agent_id, "agent-x");
        assert_eq!(row.session_id.as_deref(), Some("subtask-abc"));
        assert_eq!(row.model.as_deref(), Some("gpt-test"));
        assert_eq!(row.prompt_tokens, Some(100));
        assert_eq!(row.completion_tokens, Some(20));
        assert_eq!(row.total_tokens, Some(120));
        assert_eq!(row.cache_read_tokens, Some(30));
        assert_eq!(row.cache_creation_tokens, Some(5));
        assert_eq!(row.latency_ms, Some(42));
        assert!(row.is_bot_iteration);
        // subtask には originating Discord message が無いので必ず None。
        assert_eq!(row.trigger_message_id, None);
        assert!(row.error_code.is_none());
    }

    /// A failed sub-engine call (no response) yields null tokens but still records
    /// the error, so failures are observable instead of silently dropped.
    #[test]
    fn build_subtask_llm_log_row_records_errors_with_null_tokens() {
        let log = opencrab_core::LlmCallLog {
            request: opencrab_core::ChatRequest::new("gpt-test", vec![]),
            response: None,
            error_str: Some("boom".to_string()),
            latency_ms: 7,
            requested_at: "2026-07-25T00:00:00Z".to_string(),
            is_bot_iteration: false,
        };
        let row = build_subtask_llm_log_row("agent-x", "subtask-err", &log);
        assert_eq!(row.session_id.as_deref(), Some("subtask-err"));
        assert_eq!(row.prompt_tokens, None);
        assert_eq!(row.completion_tokens, None);
        assert_eq!(row.total_tokens, None);
        assert_eq!(row.cache_read_tokens, None);
        assert_eq!(row.error_code.as_deref(), Some("error"));
        assert_eq!(row.error_body.as_deref(), Some("boom"));
    }
}
