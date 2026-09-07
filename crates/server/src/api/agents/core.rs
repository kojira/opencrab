use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

use opencrab_actions::gateway_kinds;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub persona_name: String,
    pub image_url: Option<String>,
    pub status: String,
    pub skill_count: i32,
    pub session_count: i32,
}

pub async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT a.agent_id, a.name, a.persona_name, a.image_url,
                    (SELECT COUNT(*) FROM skills WHERE agent_id = a.agent_id) as skill_count,
                    (SELECT COUNT(*) FROM agent_sessions WHERE agent_id = a.agent_id) as session_count
             FROM agents a
             ORDER BY a.name",
        )
        .unwrap();

    let agents = stmt
        .query_map([], |row| {
            Ok(AgentSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                persona_name: row.get(2)?,
                image_url: row.get(3)?,
                status: "idle".to_string(),
                skill_count: row.get(4)?,
                session_count: row.get(5)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Json(agents)
}

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub id: Option<String>,
    pub name: String,
    pub persona_name: String,
}

pub async fn create_agent(
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Json<serde_json::Value> {
    let agent_id = req.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // workspace リゾルバ（resolve_agent_workspace）が実行時に hard-fail する id を
    // 登録時点で弾く（#48: 拒否は作成時、初回応答時ではなく）。
    if let Err(e) = opencrab_core::workspace::validate_agent_id(&agent_id) {
        return Json(serde_json::json!({"error": format!("invalid agent id: {e}")}));
    }
    let conn = state.db.lock().unwrap();

    let row = opencrab_db::queries::AgentRow {
        agent_id: agent_id.clone(),
        name: req.name.clone(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: req.persona_name,
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_agent(&conn, &row).unwrap();

    Json(serde_json::json!({
        "id": agent_id,
        "name": req.name,
    }))
}

pub async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let Some(agent) = opencrab_db::queries::get_agent(&conn, &id).unwrap() else {
        return Json(serde_json::Value::Null);
    };
    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let mut value = serde_json::to_value(agent).unwrap();
    value["subject_id"] = serde_json::json!(subject_id);
    Json(value)
}

#[derive(Debug, Deserialize)]
pub struct PutAgentBody {
    pub name: String,
    pub job_title: Option<String>,
    pub organization: Option<String>,
    pub image_url: Option<String>,
    pub persona_name: String,
    pub personality: Option<String>,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub heartbeat_instructions: String,
    pub model: Option<String>,
    pub metadata_json: Option<String>,
}

pub async fn put_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutAgentBody>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    // reasoning_effort / web_search は PUT ボディに無い。これらは AgentOverview の
    // PATCH で管理するため、identity 編集の PUT では既存値を保持する（消さない）。
    let existing = opencrab_db::queries::get_agent(&conn, &id).ok().flatten();
    let existing_effort = existing.as_ref().and_then(|a| a.reasoning_effort.clone());
    let existing_web_search = existing.as_ref().and_then(|a| a.web_search);
    // #412: model を新しい値へ変えるときだけ登録を要求する（既存値の送り直しは素通し）。
    // #676（案Y）: max_output_tokens の要求は「送るプロバイダの spec」へ切り替えるときだけ。
    // 送るか否かはプロバイダの能力宣言（router 経由）で決める（core で名前突き合わせしない）。
    let sends_max = body
        .model
        .as_deref()
        .map(|m| state.llm_router.get().sends_max_output_tokens(m))
        .unwrap_or(true);
    if let Err(e) = crate::process::check_agent_model_change(
        &conn,
        existing.as_ref(),
        body.model.as_deref(),
        sends_max,
    ) {
        return Json(serde_json::json!({"updated": false, "error": e}));
    }
    let row = opencrab_db::queries::AgentRow {
        agent_id: id,
        name: body.name,
        job_title: body.job_title,
        organization: body.organization,
        image_url: body.image_url,
        persona_name: body.persona_name,
        personality: body.personality,
        instructions: body.instructions,
        heartbeat_instructions: body.heartbeat_instructions,
        model: body.model,
        reasoning_effort: existing_effort,
        web_search: existing_web_search,
        metadata_json: body.metadata_json,
    };
    opencrab_db::queries::upsert_agent(&conn, &row).unwrap();
    Json(serde_json::json!({"updated": true}))
}

pub async fn patch_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<opencrab_db::queries::AgentPatch>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    // #412: model を実際に差し替える PATCH だけ登録を要求する。
    // クリア（既定へ戻す）は空文字で表現される（serde の `Option<Option<_>>` は
    // JSON null を「変更なし」に潰すため。`apply_agent_patch` の同趣旨のコメント参照）。
    // 空文字は `check_agent_model_change` 側で対象外になる。
    if let Some(Some(new_model)) = patch.model.as_ref() {
        let existing = opencrab_db::queries::get_agent(&conn, &id).ok().flatten();
        // #676（案Y）: 送るプロバイダの spec へ切り替えるときだけ max_output_tokens を要求。
        let sends_max = state.llm_router.get().sends_max_output_tokens(new_model);
        if let Err(e) = crate::process::check_agent_model_change(
            &conn,
            existing.as_ref(),
            Some(new_model),
            sends_max,
        ) {
            return Json(serde_json::json!({"updated": false, "error": e}));
        }
    }
    match opencrab_db::queries::apply_agent_patch(&conn, &id, &patch) {
        Ok(true) => Json(serde_json::json!({"updated": true})),
        Ok(false) => Json(serde_json::json!({"updated": false, "error": "Agent not found"})),
        Err(e) => Json(serde_json::json!({"updated": false, "error": e.to_string()})),
    }
}

pub async fn delete_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // Stop per-agent Discord gateway if running.
    if let Some(gw) = state.gateways.get(gateway_kinds::DISCORD) {
        gw.stop(&id).await;
    }

    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::delete_agent(&conn, &id).unwrap();

    Json(serde_json::json!({"deleted": deleted}))
}

// ============================================
// Soul Presets
// ============================================

pub async fn list_soul_presets(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SoulPresetRow>> {
    let conn = state.db.lock().unwrap();
    let presets = opencrab_db::queries::list_soul_presets(&conn, &id).unwrap();
    Json(presets)
}

#[derive(Debug, Deserialize)]
pub struct CreateSoulPresetRequest {
    pub preset_name: String,
}

pub async fn create_soul_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CreateSoulPresetRequest>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let agent = opencrab_db::queries::get_agent(&conn, &id).unwrap();
    let Some(agent) = agent else {
        return Json(serde_json::json!({ "ok": false, "error": "Agent not found." }));
    };

    let preset = opencrab_db::queries::SoulPresetRow {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: id,
        preset_name: req.preset_name,
        persona_name: agent.persona_name,
        custom_traits_json: agent.personality,
    };
    opencrab_db::queries::insert_soul_preset(&conn, &preset).unwrap();

    Json(serde_json::json!({ "ok": true, "id": preset.id }))
}

pub async fn delete_soul_preset(
    State(state): State<AppState>,
    Path((_id, preset_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::delete_soul_preset(&conn, &preset_id).unwrap();
    Json(serde_json::json!({ "deleted": deleted }))
}

pub async fn apply_soul_preset(
    State(state): State<AppState>,
    Path((id, preset_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let preset = opencrab_db::queries::get_soul_preset(&conn, &preset_id).unwrap();
    let Some(preset) = preset else {
        return Json(serde_json::json!({ "ok": false, "error": "Preset not found." }));
    };

    let Some(mut agent) = opencrab_db::queries::get_agent(&conn, &id).unwrap() else {
        return Json(serde_json::json!({ "ok": false, "error": "Agent not found." }));
    };
    agent.persona_name = preset.persona_name;
    agent.personality = preset.custom_traits_json;
    opencrab_db::queries::upsert_agent(&conn, &agent).unwrap();

    Json(serde_json::json!({ "ok": true }))
}

// ============================================
// Discord per-agent config
// ============================================

