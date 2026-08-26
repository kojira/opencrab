//! HTTP/SSE 外形。判断はしない。Bearer は持たない。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use super::client::{InstanceClient, LiveEvent, PostRefuse, SaidOutcome};
use super::json::parse_object_no_dup;
use super::wire::{parse_uuid, Attachment};

const JSON_TYPE: &str = "application/json; charset=utf-8";

#[derive(Clone)]
pub struct HttpState {
    pub instances: Vec<Arc<InstanceClient>>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route(
            "/api/web-conversations/{session_id}/messages",
            post(post_message),
        )
        .route(
            "/api/web-conversations/{session_id}/events",
            get(get_events),
        )
        .route("/rooms/{room}/messages", get(gone).post(gone))
        .route("/chat", get(gone))
        .with_state(state)
}

async fn gone() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn json_error(status: StatusCode, code: &str, detail: Option<&str>) -> Response {
    let body = json!({"error":{"code": code, "detail": detail}});
    let mut res = (status, Json(body)).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON_TYPE));
    res
}

fn json_state(status: StatusCode, body: Value) -> Response {
    let mut res = (status, Json(body)).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON_TYPE));
    res
}

async fn find_client(state: &HttpState, address: &str) -> Result<Arc<InstanceClient>, FindClient> {
    let mut hits = Vec::new();
    for inst in &state.instances {
        if inst.binding_for_address(address).await.is_some() {
            hits.push(inst.clone());
        }
    }
    match hits.len() {
        0 => Err(FindClient::None),
        1 => Ok(hits.remove(0)),
        _ => Err(FindClient::Ambiguous),
    }
}

async fn uds_disconnected(state: &HttpState, address: &str) -> bool {
    let mut any_live = false;
    for inst in &state.instances {
        if inst.connection_live().await {
            any_live = true;
        }
        if inst.remembered_binding(address).await.is_some() && !inst.connection_live().await {
            return true;
        }
    }
    !any_live && !state.instances.is_empty()
}

fn sse_error(code: &str) -> Response {
    let ev = Event::default()
        .event("gate_error")
        .data(json!({"code": code, "detail": null}).to_string());
    let stream = futures::stream::once(async move { Ok::<_, Infallible>(ev) });
    Sse::new(stream).into_response()
}

enum FindClient {
    None,
    Ambiguous,
}

struct PostBody {
    client_message_id: String,
    text: String,
    attachments: Vec<Attachment>,
}

fn parse_post_body(bytes: &[u8]) -> Result<PostBody, &'static str> {
    let obj = parse_object_no_dup(bytes).map_err(|_| "bad_request")?;
    let id = obj
        .get("client_message_id")
        .and_then(Value::as_str)
        .ok_or("bad_request")?;
    let client_message_id = parse_uuid(id).map_err(|_| "bad_request")?;
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .ok_or("bad_request")?;
    let attachments = parse_attachments(obj.get("attachments"))?;
    if text.is_empty() && attachments.is_empty() {
        return Err("bad_request");
    }
    Ok(PostBody {
        client_message_id,
        text: text.to_string(),
        attachments,
    })
}

fn parse_attachments(value: Option<&Value>) -> Result<Vec<Attachment>, &'static str> {
    let Some(Value::Array(items)) = value else {
        return Err("bad_request");
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object().ok_or("bad_request")?;
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("bad_request")?;
        if kind != "image" {
            return Err("bad_request");
        }
        let url = obj
            .get("url")
            .and_then(Value::as_str)
            .ok_or("bad_request")?;
        if !url.starts_with("https://") || url.len() <= "https://".len() {
            return Err("bad_request");
        }
        out.push(Attachment {
            kind: kind.to_string(),
            url: url.to_string(),
        });
    }
    Ok(out)
}

async fn post_message(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed = match parse_post_body(&body) {
        Ok(p) => p,
        Err(code) => return json_error(StatusCode::BAD_REQUEST, code, None),
    };
    let origin = format!("web:{}", parsed.client_message_id);
    let client = match find_client(&state, &session_id).await {
        Ok(c) => c,
        Err(FindClient::None) => {
            let code = if uds_disconnected(&state, &session_id).await {
                "disconnect"
            } else {
                "instance_not_ready"
            };
            return json_error(StatusCode::SERVICE_UNAVAILABLE, code, None);
        }
        Err(FindClient::Ambiguous) => {
            return json_error(StatusCode::CONFLICT, "binding_conflict", None);
        }
    };
    match client
        .post_said(&session_id, &origin, &parsed.text, &parsed.attachments)
        .await
    {
        Err(PostRefuse::NotReady) => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, "instance_not_ready", None)
        }
        Err(PostRefuse::Busy) => json_error(StatusCode::CONFLICT, "conversation_busy", None),
        Ok(SaidOutcome::Accepted { seq }) => json_state(
            StatusCode::ACCEPTED,
            json!({
                "client_message_id": parsed.client_message_id,
                "origin": origin,
                "seq": seq,
                "state": "accepted",
            }),
        ),
        Ok(SaidOutcome::NotAdmitted) => {
            json_state(StatusCode::FORBIDDEN, json!({"state": "not_admitted"}))
        }
        Ok(SaidOutcome::WireErr { code, detail }) => {
            json_error(wire_status(&code), &code, detail.as_deref())
        }
        Ok(SaidOutcome::Disconnected) => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, "disconnect", None)
        }
    }
}

fn wire_status(code: &str) -> StatusCode {
    match code {
        "bad_request" => StatusCode::BAD_REQUEST,
        "binding_unknown" | "instance_unknown" => StatusCode::NOT_FOUND,
        "instance_not_ready" | "binding_closed" | "instance_disabled" | "binding_conflict" => {
            StatusCode::CONFLICT
        }
        "store_error" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    }
}

async fn get_events(State(state): State<HttpState>, Path(session_id): Path<String>) -> Response {
    let client = match find_client(&state, &session_id).await {
        Ok(c) => c,
        Err(FindClient::None) => {
            if uds_disconnected(&state, &session_id).await {
                return sse_error("disconnect");
            }
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "instance_not_ready", None);
        }
        Err(FindClient::Ambiguous) => {
            return json_error(StatusCode::CONFLICT, "binding_conflict", None);
        }
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
    tokio::spawn(async move {
        while let Some(ev) = client.next_live(&session_id).await {
            let event = match live_to_event(&ev) {
                Some(e) => e,
                None => continue,
            };
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
            if matches!(ev, LiveEvent::Error { .. }) {
                break;
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn live_to_event(ev: &LiveEvent) -> Option<Event> {
    match ev {
        LiveEvent::Message { text } => Some(
            Event::default()
                .event("message")
                .data(json!({"text": text}).to_string()),
        ),
        LiveEvent::Activity { activity_id, state } => Some(
            Event::default()
                .event("activity")
                .data(json!({"activity_id": activity_id, "state": state}).to_string()),
        ),
        LiveEvent::CompletedNoReply => {
            Some(Event::default().event("completed_no_reply").data("{}"))
        }
        LiveEvent::Error { code, detail } => Some(
            Event::default()
                .event("gate_error")
                .data(json!({"code": code, "detail": detail}).to_string()),
        ),
    }
}
