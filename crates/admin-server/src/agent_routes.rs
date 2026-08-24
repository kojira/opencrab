//! Agents CRUD / soul_presets / a2a messages（DESIGN-DASHBOARD-P2 SLICE 2–3）。
//! handler は extract → store コマンド 1 回 → 本体封筒。SQL は書かない。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use opencrab_db::queries::{self, TrustedUserPermission, TRUSTED_PLATFORM_REST};
use opencrab_port::{EventKind, SubjectKind};
use opencrab_store::{SubjectCommandError, SubjectPatch, SubjectReplace};

use crate::api::{AdminState, ApiResult};

const UNAPPLIED_FIELDS: &[&str] = &[
    "job_title",
    "organization",
    "image_url",
    "metadata_json",
    "heartbeat_instructions",
    "reasoning_effort",
    "web_search",
];

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as i64
}

fn parse_agent(id: &str) -> ApiResult<i64> {
    id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_id",
                "detail": "id は整数（subject/place の内部 ID）である必要があります",
            })),
        )
    })
}

fn store_err(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "store_error", "detail": e.to_string() })),
    )
}

fn subject_err(error: SubjectCommandError) -> (StatusCode, Json<Value>) {
    match error {
        SubjectCommandError::EmptyIdentity => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_request",
                "detail": "name and persona_name must be non-empty",
            })),
        ),
        SubjectCommandError::Store(e) => store_err(e),
    }
}

fn soul_err(error: opencrab_store::SoulPresetError) -> (StatusCode, Json<Value>) {
    match error {
        opencrab_store::SoulPresetError::Store(e) => store_err(e),
        opencrab_store::SoulPresetError::EmptyIdentity => {
            subject_err(SubjectCommandError::EmptyIdentity)
        }
        opencrab_store::SoulPresetError::AgentMissing
        | opencrab_store::SoulPresetError::PresetMissing => (
            StatusCode::OK,
            Json(json!({ "ok": false, "error": error.to_string() })),
        ),
    }
}

/// 値つきで明示された未復元キーだけ 501。JSON null は未提供（GAP#6）。
fn reject_unapplied_valued_fields(body: &Value, fields: &[&str]) -> ApiResult<()> {
    let present: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|name| body.get(*name).is_some_and(|value| !value.is_null()))
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "unimplemented",
            "detail": format!(
                "this route does not yet apply {}: see roadmap #772",
                present.join(", ")
            ),
            "fields": present,
        })),
    ))
}

fn decode_body<T: DeserializeOwned>(body: Value) -> ApiResult<T> {
    serde_json::from_value(body).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "bad_request", "detail": e.to_string() })),
        )
    })
}

#[derive(Debug, Deserialize)]
struct CreateAgentRequest {
    pub id: Option<String>,
    pub name: String,
    pub persona_name: String,
}

#[derive(Debug, Deserialize)]
struct PutAgentBody {
    pub name: String,
    pub persona_name: String,
    pub personality: Option<String>,
    #[serde(default)]
    pub instructions: String,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct PatchAgentBody {
    pub name: Option<String>,
    pub persona_name: Option<String>,
    pub personality: Option<String>,
    pub instructions: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendAgentMessageRequest {
    pub content: String,
    pub user_id: String,
}

#[derive(Debug, Deserialize)]
struct CreateSoulPresetRequest {
    pub preset_name: String,
}

async fn create_agent(
    State(st): State<AdminState>,
    Json(req): Json<CreateAgentRequest>,
) -> ApiResult<Json<Value>> {
    let explicit = match req.id.as_deref() {
        Some(raw) => Some(parse_agent(raw)?),
        None => None,
    };
    let id = st
        .store
        .subject_create(explicit, &req.name, &req.persona_name, now_ns())
        .map_err(subject_err)?;
    Ok(Json(json!({ "id": id.to_string(), "name": req.name })))
}

async fn put_agent(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    reject_unapplied_valued_fields(&body, UNAPPLIED_FIELDS)?;
    let req: PutAgentBody = decode_body(body)?;
    let agent = parse_agent(&id)?;
    let updated = st
        .store
        .subject_replace(
            agent,
            &SubjectReplace {
                name: req.name,
                persona_name: req.persona_name,
                personality: req.personality,
                instructions: req.instructions,
                model: req.model,
            },
            now_ns(),
        )
        .map_err(subject_err)?;
    Ok(Json(json!({ "updated": updated })))
}

async fn patch_agent(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    reject_unapplied_valued_fields(&body, UNAPPLIED_FIELDS)?;
    let req: PatchAgentBody = decode_body(body)?;
    let agent = parse_agent(&id)?;
    let updated = st
        .store
        .subject_patch(
            agent,
            &SubjectPatch {
                name: req.name,
                persona_name: req.persona_name,
                personality: req.personality,
                instructions: req.instructions,
                model: req.model,
            },
            now_ns(),
        )
        .map_err(subject_err)?;
    if updated {
        Ok(Json(json!({ "updated": true })))
    } else {
        Ok(Json(
            json!({ "updated": false, "error": "Agent not found" }),
        ))
    }
}

async fn delete_agent(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    let deleted = st
        .store
        .subject_delete(agent, now_ns())
        .map_err(subject_err)?;
    Ok(Json(json!({ "deleted": deleted })))
}

fn caller_type(st: &AdminState, agent: i64, user_id: &str) -> &'static str {
    let Ok(conn) = st.db.lock() else {
        return "agent";
    };
    match queries::get_trusted_user(&conn, TRUSTED_PLATFORM_REST, user_id, &agent.to_string()) {
        Some(row) => match row.permission {
            TrustedUserPermission::Owner => "owner",
            TrustedUserPermission::CoAgent => "co_agent",
            TrustedUserPermission::User => "trusted_user",
        },
        None => "agent",
    }
}

async fn wait_spoke(st: &AdminState, place: i64, after: i64) -> ApiResult<String> {
    loop {
        let last = st.store.latest_seq(place).map_err(store_err)?;
        if last > after {
            let events = st.store.read_range(place, after, last).map_err(store_err)?;
            if let Some(spoke) = events
                .into_iter()
                .find(|event| event.kind == EventKind::Spoke)
            {
                return Ok(spoke.content.text.unwrap_or_default());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn send_agent_message(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<SendAgentMessageRequest>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    match st.store.get_subject(agent).map_err(store_err)? {
        Some(row) if row.kind == SubjectKind::Agent => {}
        _ => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "not_found", "detail": "agent がありません" })),
            ));
        }
    }
    let user_id = req.user_id.trim();
    let caller_type = caller_type(&st, agent, user_id);
    let Some(out) = st
        .store
        .agent_direct_message(agent, user_id, &req.content, now_ns())
        .map_err(subject_err)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not_found", "detail": "agent がありません" })),
        ));
    };
    let content = wait_spoke(&st, out.place_id, out.said_seq).await?;
    Ok(Json(json!({
        "session_id": out.session_id,
        "caller_type": caller_type,
        "responses": [{
            "agent_id": id,
            "content": content,
        }],
    })))
}

async fn list_soul_presets(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    let rows = st.store.soul_preset_list(agent).map_err(soul_err)?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|row| json!({
            "id": row.id,
            "agent_id": row.agent_id,
            "preset_name": row.preset_name,
            "persona_name": row.persona_name,
            "custom_traits_json": row.custom_traits_json,
        }))
        .collect::<Vec<_>>())))
}

async fn create_soul_preset(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<CreateSoulPresetRequest>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    match st
        .store
        .soul_preset_create(agent, &req.preset_name, now_ns())
    {
        Ok(preset_id) => Ok(Json(json!({ "ok": true, "id": preset_id }))),
        Err(opencrab_store::SoulPresetError::AgentMissing) => {
            Ok(Json(json!({ "ok": false, "error": "Agent not found." })))
        }
        Err(error) => Err(soul_err(error)),
    }
}

async fn delete_soul_preset(
    State(st): State<AdminState>,
    Path((_id, preset_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let deleted = st.store.soul_preset_delete(&preset_id).map_err(soul_err)?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn apply_soul_preset(
    State(st): State<AdminState>,
    Path((id, preset_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    match st.store.soul_preset_apply(agent, &preset_id, now_ns()) {
        Ok(()) => Ok(Json(json!({ "ok": true }))),
        Err(opencrab_store::SoulPresetError::AgentMissing) => {
            Ok(Json(json!({ "ok": false, "error": "Agent not found." })))
        }
        Err(opencrab_store::SoulPresetError::PresetMissing) => {
            Ok(Json(json!({ "ok": false, "error": "Preset not found." })))
        }
        Err(error) => Err(soul_err(error)),
    }
}

pub fn agent_write_routes() -> Router<AdminState> {
    Router::new()
        .route("/api/agents", axum::routing::post(create_agent))
        .route(
            "/api/agents/{id}",
            axum::routing::put(put_agent)
                .patch(patch_agent)
                .delete(delete_agent),
        )
        .route(
            "/api/agents/{id}/messages",
            axum::routing::post(send_agent_message),
        )
        .route(
            "/api/agents/{id}/soul/presets",
            get(list_soul_presets).post(create_soul_preset),
        )
        .route(
            "/api/agents/{id}/soul/presets/{pid}",
            axum::routing::delete(delete_soul_preset),
        )
        .route(
            "/api/agents/{id}/soul/presets/{pid}/apply",
            axum::routing::post(apply_soul_preset),
        )
}

#[cfg(test)]
mod contract {
    use super::*;
    use crate::api::{create_router, AdminState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_db::Db;
    use opencrab_port::{
        Content, GateInstanceId, GateKindId, IngressDiscovery, OriginScope, Standing,
    };
    use opencrab_store::{NewEvent, Store};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn dummy_db() -> Arc<Db> {
        Arc::new(Db::from_connection(
            rusqlite::Connection::open_in_memory().expect("memory db"),
        ))
    }

    fn state_from_store(store: Store) -> AdminState {
        AdminState {
            store: Arc::new(store),
            db: dummy_db(),
            compaction_ratio: 0.5,
        }
    }

    async fn call(
        state: AdminState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(value) => Body::from(serde_json::to_vec(&value).expect("json")),
                None => Body::empty(),
            })
            .expect("request");
        let response = create_router(state)
            .oneshot(request)
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn create_put_patch_delete_body_envelopes() {
        let state = state_from_store(Store::new_in_memory().expect("store"));
        let (status, body) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["id"].as_str().expect("id").to_string();
        assert_eq!(body["name"], "Ada");

        let (status, got) = call(state.clone(), "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(got["name"], "Ada");
        assert_eq!(got["persona_name"], "Helper");
        assert_eq!(got["instructions"], "");

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{id}"),
            Some(json!({
                "name":"Bea",
                "persona_name":"Guide",
                "personality":"curious",
                "instructions":"be brief",
                "model":"provider:model"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"updated": true}));
        let (_, got) = call(state.clone(), "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(got["name"], "Bea");
        assert_eq!(got["persona_name"], "Guide");
        assert_eq!(got["personality"], "curious");
        assert_eq!(got["instructions"], "be brief");
        assert_eq!(got["model"], "provider:model");

        let (status, body) = call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({"name":"Cara"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"updated": true}));
        let (_, got) = call(state.clone(), "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(got["name"], "Cara");
        assert_eq!(got["persona_name"], "Guide");

        let (status, body) =
            call(state.clone(), "DELETE", &format!("/api/agents/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"deleted": true}));
        let (_, got) = call(state, "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(got, Value::Null);
    }

    #[tokio::test]
    async fn patch_null_is_not_provided_and_valued_unrestored_is_501() {
        let state = state_from_store(Store::new_in_memory().expect("store"));
        let (_, created) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let (status, body) = call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({
                "name":"Bea",
                "job_title":null,
                "organization":null,
                "image_url":null,
                "metadata_json":null
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["updated"], true);
        let (_, got) = call(state.clone(), "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(got["name"], "Bea");

        let (status, body) = call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({"organization":"Acme"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
        assert!(body["detail"]
            .as_str()
            .unwrap_or("")
            .contains("organization"));
        assert!(body["detail"].as_str().unwrap_or("").contains("#772"));
        let (_, got) = call(state.clone(), "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(got["name"], "Bea");

        let (status, body) = call(
            state,
            "PUT",
            &format!("/api/agents/{id}"),
            Some(json!({
                "name":"Bea",
                "persona_name":"Helper",
                "job_title":"Dev"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert!(body["detail"].as_str().unwrap_or("").contains("job_title"));
    }

    #[tokio::test]
    async fn messages_absent_agent_is_404() {
        let state = state_from_store(Store::new_in_memory().expect("store"));
        let (status, body) = call(
            state,
            "POST",
            "/api/agents/99/messages",
            Some(json!({"content":"hi","user_id":"u1"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "not_found");
    }

    #[tokio::test]
    async fn messages_body_envelope_after_spoke() {
        let store = Store::new_in_memory().expect("store");
        let state = state_from_store(store);
        let (_, created) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        let writer = state.store.clone();
        let agent: i64 = id.parse().unwrap();
        std::thread::spawn(move || loop {
            if let Ok(places) = writer.all_places() {
                for place in places {
                    if let Ok(last) = writer.latest_seq(place.id) {
                        if last >= 1 {
                            if let Ok(events) = writer.read_range(place.id, 0, last) {
                                if events.iter().any(|event| event.kind == EventKind::Said)
                                    && !events.iter().any(|event| event.kind == EventKind::Spoke)
                                    && writer
                                        .append(
                                            place.id,
                                            &NewEvent {
                                                kind: EventKind::Spoke,
                                                author_subject: Some(agent),
                                                author_external: None,
                                                content: Content::text("pong"),
                                                mentions: vec![],
                                                reply_to: None,
                                                target: None,
                                                for_subject: None,
                                                attachments: vec![],
                                                metadata: json!({}),
                                            },
                                            30,
                                        )
                                        .is_ok()
                                {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        });
        let (status, body) = call(
            state,
            "POST",
            &format!("/api/agents/{id}/messages"),
            Some(json!({"content":"hello","user_id":"  rest-user  "})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_id"], format!("agent-msg-{id}-rest-user"));
        assert_eq!(body["caller_type"], "agent");
        assert_eq!(body["responses"][0]["agent_id"], id);
        assert_eq!(body["responses"][0]["content"], "pong");
    }

    #[tokio::test]
    async fn soul_presets_body_envelopes_and_apply() {
        let state = state_from_store(Store::new_in_memory().expect("store"));
        let (_, created) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();
        call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({"personality":"curious"})),
        )
        .await;
        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/soul/presets"),
            Some(json!({"preset_name":"saved"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        let pid = body["id"].as_str().unwrap().to_string();
        let (status, listed) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{id}/soul/presets"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed[0]["preset_name"], "saved");
        call(
            state.clone(),
            "PATCH",
            &format!("/api/agents/{id}"),
            Some(json!({"persona_name":"Guide","personality":""})),
        )
        .await;
        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/soul/presets/{pid}/apply"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"ok": true}));
        let (_, got) = call(state.clone(), "GET", &format!("/api/agents/{id}"), None).await;
        assert_eq!(got["persona_name"], "Helper");
        assert_eq!(got["personality"], "curious");
        let (status, body) = call(
            state,
            "DELETE",
            &format!("/api/agents/{id}/soul/presets/{pid}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"deleted": true}));
    }

    #[tokio::test]
    async fn delete_scopes_tombstone_to_discord_only() {
        let store = Store::new_in_memory().expect("store");
        let agent = store
            .create_subject(
                SubjectKind::Agent,
                "A",
                "persona",
                "engine",
                Standing::Trusted,
                0,
            )
            .expect("agent");
        let config = serde_json::to_vec(&json!({
            "agent_ids": [],
            "legacy_updated_at": "",
            "owner_external_id": "",
            "self_external_id": null,
        }))
        .unwrap();
        for (kind, instance) in [
            ("discord", "018f8020-0000-7000-8000-000000000021"),
            ("nostr", "018f8020-0000-7000-8000-000000000022"),
        ] {
            store
                .install_gate_instance_revision(
                    &GateInstanceId::parse(instance.to_string()).unwrap(),
                    &GateKindId::parse(kind.to_string()).unwrap(),
                    &format!("dedicated:{kind}:{agent}"),
                    Some(agent),
                    1,
                    true,
                    OriginScope::KindAddress,
                    IngressDiscovery::Membership,
                    &format!("gate-config/{kind}/v1"),
                    &config,
                    12,
                )
                .unwrap();
        }
        let state = state_from_store(store);
        let (status, body) = call(
            state.clone(),
            "DELETE",
            &format!("/api/agents/{agent}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"deleted": true}));
        let present = |kind: &str| {
            state
                .store
                .dedicated_gate_instance(&GateKindId::parse(kind.to_string()).unwrap(), agent)
                .unwrap()
                .and_then(|instance| state.store.gate_owner_projection(&instance).unwrap())
                .map(|proj| proj.present)
        };
        assert_eq!(present("discord"), Some(false));
        assert_eq!(present("nostr"), Some(true));
    }
}
