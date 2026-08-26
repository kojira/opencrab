use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use opencrab_actions::{
    accept_inbound, prepare_session_inbound_write, run_session_turn, AgentRuntime, InboundLookups,
    InboundWork, NormalizedInbound, NormalizedInboundEvent,
    PrepareSessionInboundError, RunRequest,
};
use serde::Deserialize;

use crate::process;
use crate::process::AgentNotFound;
use crate::AppState;

/// web/send と同じ番兵。`accept_inbound` は `guild_id` が空のときだけ DM ゲートを見る。
/// 値は `opencrab_web_gateway::WEB_INBOUND_GUILD` と一致させる。
const OWNER_INBOUND_GUILD: &str = "web";
/// SessionDetail は `user_id` を送らない。web/send 省略時と同じ主体。
/// 値は `opencrab_web_gateway::DEFAULT_WEB_USER_ID` と一致させる。
const OWNER_USER_ID: &str = "web-user";

pub async fn list_session_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SessionLogRow>> {
    let conn = state.db.lock().unwrap();
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, &id).unwrap_or_default();
    Json(logs)
}

#[derive(Debug, Deserialize)]
pub struct MentorInstructionRequest {
    pub content: String,
}

pub async fn send_mentor_instruction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MentorInstructionRequest>,
) -> Json<serde_json::Value> {
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: "mentor".to_string(),
        session_id: id,
        log_type: "system".to_string(),
        content: req.content,
        speaker_id: Some("mentor".to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    let conn = state.db.lock().unwrap();
    let log_id = match opencrab_db::queries::insert_session_log(&conn, &log) {
        Ok(id) => id,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to record mentor instruction: {e}")
            }));
        }
    };
    Json(serde_json::json!({"id": log_id}))
}

#[derive(Debug, Deserialize)]
pub struct OwnerInstructionRequest {
    pub content: String,
}

/// Dashboard SessionDetail のオーナー指示（`POST /api/sessions/{id}/owner`）。
///
/// 判断は core の [`accept_inbound`] 1 口。記録は
/// [`prepare_session_inbound_write`]。ターン起動は [`run_session_turn`]。
/// ゲートは HTTP 抽出と配線だけ。
pub async fn send_owner_instruction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<OwnerInstructionRequest>,
) -> Response {
    let (session_theme, participant_ids) = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "database unavailable"})),
                )
                    .into_response();
            }
        };
        let session = match opencrab_db::queries::get_session(&conn, &id) {
            Ok(Some(session)) => session,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": format!("session not found: {id}")})),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("failed to load session: {e}")})),
                )
                    .into_response();
            }
        };
        let participant_ids = match opencrab_db::queries::list_session_participants(&conn, &id) {
            Ok(ids) if !ids.is_empty() => ids,
            Ok(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "session has no participants"})),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to load participants: {e}")
                    })),
                )
                    .into_response();
            }
        };
        (session.theme, participant_ids)
    };

    // InboundLookups の dyn Fn は Send でないので、await の前にブロックで落とす。
    let (caller, admitted_ids, should_run) = {
        let inbound_event = NormalizedInboundEvent {
            sender_id: OWNER_USER_ID,
            channel_id: "",
            guild_id: OWNER_INBOUND_GUILD,
        };
        let work = InboundWork {
            event: inbound_event,
            has_content: !req.content.trim().is_empty(),
            kind_label: "",
            author_key: OWNER_USER_ID,
        };
        let resolve = |s: &str, a: &[String], _: &str| {
            let agent_id = a
                .first()
                .expect("owner inbound は participants 確認後にだけ呼ぶ");
            let conn = state.db.lock().unwrap();
            crate::caller_identity::resolve_caller_identity(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_WEB,
                s,
                agent_id,
            )
        };
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let mut caller = None;
        let mut admitted_ids = Vec::new();
        let mut should_run = false;
        accept_inbound::<()>(
            &[work],
            "",
            &participant_ids,
            &lookups,
            None,
            |_| (),
            |_, adm| {
                caller = Some(adm.caller.clone());
                admitted_ids = adm.admitted_agent_ids.clone();
            },
            |_, _, _| should_run = true,
        )
        .expect("owner inbound は DM ではない（OWNER_INBOUND_GUILD が非空）");
        (
            caller.expect("owner inbound は 1 件通る"),
            admitted_ids,
            should_run,
        )
    };

    let record_agent_id = participant_ids[0].as_str();
    let inbound = NormalizedInbound {
        session_id: &id,
        agent_id: record_agent_id,
        sender_id: OWNER_USER_ID,
        sender_name: "",
        avatar_url: None,
        channel_id: None,
        pubkey: None,
        text: &req.content,
        image_urls: &[],
        external_id: "",
    };
    let mut log_id = 0i64;
    if let Err(e) = prepare_session_inbound_write(
        &inbound,
        |_, _| Ok(()),
        |aid, sid, uid, content| {
            let conn = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("database unavailable: {e}"))?;
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: aid.to_string(),
                session_id: sid.to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(uid.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            };
            log_id = opencrab_db::queries::insert_session_log(&conn, &log)?;
            Ok(())
        },
    ) {
        let error = match e {
            PrepareSessionInboundError::Ensure(e) => format!("Failed to create session: {e}"),
            PrepareSessionInboundError::Record(e) => format!("Failed to log message: {e}"),
        };
        return Json(serde_json::json!({"error": error})).into_response();
    }

    if !should_run || state.llm_router.get().provider_names().is_empty() {
        return Json(serde_json::json!({
            "id": log_id,
            "session_id": id,
        }))
        .into_response();
    }

    let mut responses = Vec::new();
    for agent_id in &admitted_ids {
        let (system_prompt, agent_name) = state.build_agent_context(agent_id, &caller);
        let result = state
            .session_locks
            .run_serialized(
                &id,
                run_session_turn(
                    &state,
                    &id,
                    agent_id,
                    |raw| process::prepend_runtime_context(raw, &session_theme),
                    |conversation| {
                        RunRequest::new(
                            agent_id,
                            &agent_name,
                            &id,
                            &system_prompt,
                            &conversation,
                            "rest",
                            caller.clone(),
                        )
                    },
                ),
            )
            .await;

        match result {
            Some(Ok(engine_result)) => {
                {
                    let conn = state.db.lock().unwrap();
                    crate::transcript::record_rest_agent_reply(
                        &conn,
                        agent_id,
                        &id,
                        &engine_result.response,
                        engine_result.iterations,
                        engine_result.tool_calls_made,
                    );
                }
                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": engine_result.response,
                    "tool_calls_made": engine_result.tool_calls_made,
                }));
            }
            Some(Err(e)) => {
                if let Some(nf) = e.downcast_ref::<AgentNotFound>() {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"error": nf.to_string()})),
                    )
                        .into_response();
                }
                tracing::error!(agent_id = %agent_id, error = %e, "SkillEngine failed");
                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": format!("(Error: {})", e),
                    "tool_calls_made": 0,
                }));
            }
            None => {}
        }
    }

    Json(serde_json::json!({
        "id": log_id,
        "session_id": id,
        "responses": responses,
    }))
    .into_response()
}

pub async fn list_sessions(
    State(state): State<AppState>,
) -> Json<Vec<opencrab_db::queries::SessionRow>> {
    let conn = state.db.lock().unwrap();
    let sessions = opencrab_db::queries::list_sessions(&conn).unwrap_or_default();
    Json(sessions)
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub theme: String,
    pub mode: Option<String>,
    pub participant_ids: Vec<String>,
    pub max_turns: Option<i32>,
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Json<serde_json::Value> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let session = opencrab_db::queries::SessionRow {
        id: session_id.clone(),
        mode: req.mode.unwrap_or_else(|| "autonomous".to_string()),
        theme: req.theme,
        phase: "divergent".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::to_string(&req.participant_ids).unwrap(),
        facilitator_id: None,
        done_count: 0,
        max_turns: req.max_turns,
        metadata_json: None,
    };

    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_session(&conn, &session).unwrap();

    Json(serde_json::json!({
        "id": session_id,
    }))
}

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let session = opencrab_db::queries::get_session(&conn, &id).unwrap();
    Json(serde_json::to_value(session).unwrap())
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub agent_id: String,
    pub content: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Response {
    // 1. Log the sender's message to DB.
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: req.agent_id.clone(),
        session_id: id.clone(),
        log_type: "speech".to_string(),
        content: req.content.clone(),
        speaker_id: Some(req.agent_id.clone()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };

    let log_id = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return Json(serde_json::json!({"error": "database unavailable"})).into_response();
            }
        };
        match opencrab_db::queries::insert_session_log(&conn, &log) {
            Ok(id) => id,
            Err(e) => {
                return Json(serde_json::json!({"error": format!("failed to log message: {e}")}))
                    .into_response();
            }
        }
    };

    // 2. Check if LLM providers are available. If none, fall back to legacy behavior.
    if state.llm_router.get().provider_names().is_empty() {
        return Json(serde_json::json!({
            "id": log_id,
            "session_id": id,
        }))
        .into_response();
    }

    // 3. Get session and participant IDs.
    let session = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return Json(serde_json::json!({"error": "database unavailable"})).into_response();
            }
        };
        match opencrab_db::queries::get_session(&conn, &id) {
            Ok(Some(session)) => session,
            Ok(None) => {
                return Json(serde_json::json!({"error": format!("session not found: {id}")}))
                    .into_response();
            }
            Err(e) => {
                return Json(serde_json::json!({"error": format!("failed to load session: {e}")}))
                    .into_response();
            }
        }
    };
    // 参加者は agent_sessions テーブルが正（#37）。JSON 列は wire 契約用の投影。
    let participant_ids: Vec<String> = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return Json(serde_json::json!({"error": "database unavailable"})).into_response()
            }
        };
        opencrab_db::queries::list_session_participants(&conn, &id).unwrap_or_default()
    };
    let session_theme = session.theme;

    // 4. For each participant (except the sender), run SkillEngine.
    let mut responses = Vec::new();

    for agent_id in &participant_ids {
        if agent_id == &req.agent_id {
            continue;
        }

        // Build agent context from DB. この経路の run は caller=Owner（下の RunRequest と
        // 一致）。Owner には全 skill を見せる（#352）。
        let (system_prompt, agent_name) = {
            let conn = state.db.lock().unwrap();
            process::build_agent_context(&conn, agent_id, &opencrab_actions::CallerIdentity::Owner)
        };

        // Build conversation history from session logs.
        let conversation = {
            let conn = state.db.lock().unwrap();
            let eff = opencrab_db::queries::effective_model_for_agent(
                &conn,
                agent_id,
                &state.default_model,
            )
            .unwrap_or_else(|_| state.default_model.clone());
            let (prov, mdl) = process::split_llm_model_spec(&eff);
            let budget = process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
            let raw = match process::build_conversation_string(&conn, &id, agent_id, budget) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(agent_id = %agent_id, session_id = %id, "build_conversation_string failed: {e}");
                    continue;
                }
            };
            process::prepend_runtime_context(&raw, &session_theme)
        };

        // Run agent through the shared pipeline.
        //
        // 同一セッションへの並行 POST を直列化する（#640）。共有ロック
        // （`state.session_locks`）を session（= path の `id`）単位で被せる。
        // これは判断ではなく配線漏れの解消で、この経路も `run_agent_response` を直呼びしていた。
        //
        // 粒度は **session_id 単位であって global ではない**。同一セッションの run だけが直列化
        // され、別セッションへの POST は従来どおり並行に走る。粒度を広げないこと。
        let result = state
            .session_locks
            .run_serialized(
                &id,
                process::run_agent_response(
                    &state,
                    opencrab_actions::RunRequest::new(
                        agent_id,
                        &agent_name,
                        &id,
                        &system_prompt,
                        &conversation,
                        "rest",
                        opencrab_actions::CallerIdentity::Owner,
                    ),
                ),
            )
            .await;

        match result {
            Ok(engine_result) => {
                // Log the agent's response to DB.
                {
                    let conn = state.db.lock().unwrap();
                    crate::transcript::record_rest_agent_reply(
                        &conn,
                        agent_id,
                        &id,
                        &engine_result.response,
                        engine_result.iterations,
                        engine_result.tool_calls_made,
                    );
                }

                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": engine_result.response,
                    "tool_calls_made": engine_result.tool_calls_made,
                }));
            }
            Err(e) => {
                // #632: 存在しない participant はチョークポイント（run_agent_response）で
                // 弾かれる。ターンは走らない。他 participant の結果を混ぜず 404 に写像する
                // （でたらめな participant を含むセッションへの send を無効な要求として扱う）。
                if let Some(nf) = e.downcast_ref::<AgentNotFound>() {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"error": nf.to_string()})),
                    )
                        .into_response();
                }
                tracing::error!(agent_id = %agent_id, error = %e, "SkillEngine failed");
                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": format!("(Error: {})", e),
                    "tool_calls_made": 0,
                }));
            }
        }
    }

    Json(serde_json::json!({
        "id": log_id,
        "session_id": id,
        "responses": responses,
    }))
    .into_response()
}
