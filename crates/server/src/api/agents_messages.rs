use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use opencrab_actions::{SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled};

use crate::process;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SendAgentMessageRequest {
    pub content: String,
    pub user_id: String,
}

/// REST 経路のセッション ID 接頭辞（`agent-msg-{agent_id}-{user_id}`）。
pub const REST_SESSION_PREFIX: &str = "agent-msg-";

/// REST 経路の完了 sink（#169）。**resume も live push もしない「保存のみ」**。
///
/// 完了本文は `settle_completed` が親セッションログへ永続化済み（RFC §1.3）なので、
/// 取得は `GET /api/sessions/{id}/logs` で足りる。ここで LLM を回して結果を再注入
/// （web の `WebCompletionSink` 相当）はしない:
/// - REST には per-session の直列化が無い（web は `WebGateway::run_serialized`、Discord は
///   `spawn_serialized_on_session` を持つ）。resume を走らせると同一セッションへの
///   並行 POST と競合し、同じ会話から二重に応答する（RFC §6 の不変条件違反）。
/// - 完了本文は次の POST の `build_conversation_string` で自然に文脈へ載る
///   （heartbeat の「次 tick 拾い」と同じ方式）。
///
/// sink の役目は `sessions.status` の整合だけ: 走行中 subtask がある間はハンドラが
/// `completed` にしないため、最後の subtask が決着した時点でここが完了させる
/// （そうしないとセッションが永久に `active` のまま残る）。
pub struct RestCompletionSink {
    pub db: opencrab_db::Db,
    /// この REST セッションの共有 registry（他に走行中 subtask が無いかの判定用）。
    pub registry: SubtaskRegistry,
}

impl SubtaskCompletionSink for RestCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        if !ev.session_id.starts_with(REST_SESSION_PREFIX) {
            tracing::debug!(
                session_id = %ev.session_id,
                "rest sink: parent session is not a REST session, nothing to reconcile"
            );
            return;
        }
        // `settle_completed` は sink 発火より前に当該 subtask を registry から除去する。
        // 空でなければ他の subtask がまだ走行中 = セッションは完了ではない。
        if !self.registry.is_empty() {
            tracing::debug!(
                session_id = %ev.session_id,
                running = self.registry.len(),
                "rest sink: subtask settled but others are still running"
            );
            return;
        }
        mark_session_completed(&self.db, &ev.session_id);
    }
}

/// セッションを `completed` にする（best-effort）。
fn mark_session_completed(db: &opencrab_db::Db, session_id: &str) {
    if let Ok(conn) = db.lock() {
        conn.execute(
            "UPDATE sessions SET status = 'completed' WHERE id = ?1",
            [session_id],
        )
        .ok();
    }
}

/// 走行中の dispatch subtask が無いときだけセッションを `completed` にする（#169）。
///
/// 非ブロック dispatch により、HTTP 応答を返した時点でまだ background subtask が
/// 走っていることがある。そこで `completed` にすると「完了したのに走行中」という
/// 不整合になるため、走行中は `active` のまま残し、最後の subtask が決着した時点で
/// `RestCompletionSink` が完了させる。
fn complete_session_if_idle(db: &opencrab_db::Db, session_id: &str, registry: &SubtaskRegistry) {
    if !registry.is_empty() {
        tracing::debug!(
            session_id = %session_id,
            running = registry.len(),
            "rest: subtask still running, keeping session active"
        );
        return;
    }
    mark_session_completed(db, session_id);
}

pub async fn send_agent_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendAgentMessageRequest>,
) -> Json<serde_json::Value> {
    // 呼び出し元 ID は入口で 1 回だけ正規化し、以降すべて（認可・セッションキー・
    // speaker_id）で同じ値を使う。`is_owner_id` が trim して比較する一方でセッション
    // キーだけ生値を使うと、`" <id> "` が owner にはなれるのに別セッション・別
    // speaker_id として記録される非対称が生まれる。
    let user_id = req.user_id.trim();
    let session_id = format!("{}{}-{}", REST_SESSION_PREFIX, id, user_id);

    // 1. Determine caller identity from trusted_users table.
    let caller = {
        let conn = state.db.lock().unwrap();
        match opencrab_db::queries::get_trusted_user(&conn, user_id, &id) {
            Some(u) if u.permission == "co_agent" => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: user_id.to_string(),
            },
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
            None => {
                let cfg = opencrab_db::queries::get_agent_discord_config(&conn, &id);
                if let Ok(Some(c)) = cfg {
                    if crate::api::is_owner_id(&c.owner_discord_id, user_id) {
                        opencrab_actions::CallerIdentity::Owner
                    } else {
                        opencrab_actions::CallerIdentity::Agent
                    }
                } else {
                    opencrab_actions::CallerIdentity::Agent
                }
            }
        }
    };

    let caller_type = match &caller {
        opencrab_actions::CallerIdentity::CoAgent { .. } => "co_agent",
        opencrab_actions::CallerIdentity::TrustedUser => "trusted_user",
        opencrab_actions::CallerIdentity::Owner => "owner",
        _ => "agent",
    };

    // 2. Ensure session exists (create if not).
    {
        let conn = state.db.lock().unwrap();
        let existing = opencrab_db::queries::get_session(&conn, &session_id)
            .ok()
            .flatten();
        if existing.is_none() {
            let session = opencrab_db::queries::SessionRow {
                id: session_id.clone(),
                mode: "autonomous".to_string(),
                theme: "direct_message".to_string(),
                phase: "divergent".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!([&id]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            };
            if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
                return Json(
                    serde_json::json!({"error": format!("Failed to create session: {}", e)}),
                );
            }
        }
    }

    // 3. Log user message.
    {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: id.clone(),
            session_id: session_id.clone(),
            log_type: "speech".to_string(),
            content: req.content.clone(),
            speaker_id: Some(user_id.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let conn = state.db.lock().unwrap();
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            return Json(serde_json::json!({"error": format!("Failed to log message: {}", e)}));
        }
    }

    // 4. Check LLM availability.
    if state.llm_router.get().provider_names().is_empty() {
        return Json(serde_json::json!({
            "session_id": session_id,
            "caller_type": caller_type,
            "responses": [],
            "error": "No LLM providers available",
        }));
    }

    // 5. dispatch 用の共有 registry を確保する（#169）。
    //    `AppState` が session_id キーで保持しているものを借りるため、リクエストを
    //    跨いでも同一 Arc であり、dispatch した subtask を後続リクエストの
    //    `cancel_subtask` から停止できる（使い捨ての DashMap では常に not found）。
    let subtask_registry = state.subtask_registries.registry_for(&session_id);

    // 6. Get gateway_actions from discord_manager.
    //    Discord の gateway_actions にも**同一の**registry を渡す。
    //    `SystemGatewayActions` は inner が cancel_subtask を実装している場合そちらへ
    //    委譲するため、別 registry を渡すと停止が not found になる。
    #[cfg(feature = "discord")]
    let gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> = {
        if let Some(ref dm) = state.discord_manager {
            if let Some(http) = dm.get_http_for_agent(&id) {
                let tools_cfg = state.tools_config.read().unwrap().clone();
                let subtask_registry = subtask_registry.clone();
                let ga = opencrab_discord::DiscordGatewayActions::new(
                    http,
                    state.db.clone(),
                    Arc::new(std::sync::RwLock::new(tools_cfg)),
                    None,
                    state.default_model.clone(),
                    state.workspace_base.clone(),
                    subtask_registry,
                    None,
                );
                Some(Arc::new(ga) as Arc<dyn opencrab_gateway::GatewayActions>)
            } else {
                None
            }
        } else {
            None
        }
    };
    #[cfg(not(feature = "discord"))]
    let gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> = None;

    // 7. Build agent context.
    let (system_prompt, agent_name) = {
        let conn = state.db.lock().unwrap();
        process::build_agent_context(&conn, &id)
    };

    // 8. Build conversation string.
    let conversation = {
        let conn = state.db.lock().unwrap();
        let eff = opencrab_db::queries::effective_model_for_agent(&conn, &id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone());
        let (prov, mdl) = process::split_llm_model_spec(&eff);
        let budget = process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
        let raw = match process::build_conversation_string(&conn, &session_id, &id, budget) {
            Ok(s) => s,
            Err(e) => {
                return Json(
                    serde_json::json!({"error": format!("Failed to build conversation: {}", e)}),
                );
            }
        };
        process::prepend_runtime_context(&raw, "direct_message")
    };

    // 9. Run agent response.
    //     非ブロック dispatch（#152 S3a / #169）を有効化する。長時間ツールは background
    //     subtask になり、HTTP 応答はメインを塞がずに即座に返る。sink は「保存のみ」
    //     （resume / live push なし。取得はセッションログ経由）。
    let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(RestCompletionSink {
        db: state.db.clone(),
        registry: subtask_registry.clone(),
    });
    let mut run_req = opencrab_actions::RunRequest::new(
        &id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "rest",
        caller,
    )
    .with_dispatch(Some(subtask_registry.clone()), sink);
    if let Some(ga) = gateway_actions {
        run_req = run_req.with_gateway_actions(ga);
    }
    let result = process::run_agent_response(&state, run_req).await;

    // 10. Handle result.
    match result {
        Ok(engine_result) => {
            // Log agent response.
            {
                let conn = state.db.lock().unwrap();
                crate::transcript::record_rest_agent_reply(
                    &conn,
                    &id,
                    &session_id,
                    &engine_result.response,
                    engine_result.iterations,
                    engine_result.tool_calls_made,
                );
            }

            // 走行中 subtask が無ければセッションを完了にする（走行中は active のまま。
            // 最後の subtask 決着時に RestCompletionSink が完了させる）。
            complete_session_if_idle(&state.db, &session_id, &subtask_registry);

            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": [{
                    "agent_id": id,
                    "content": engine_result.response,
                }],
            }))
        }
        Err(e) => {
            tracing::error!(agent_id = %id, error = %e, "Agent response failed");
            // エラー時も同様: 走行中 subtask（エラー前に dispatch 済み）があれば
            // 完了扱いにしない。
            complete_session_if_idle(&state.db, &session_id, &subtask_registry);
            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": [{
                    "agent_id": id,
                    "content": format!("(Error: {})", e),
                }],
            }))
        }
    }
}
