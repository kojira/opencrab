//! 既存 Nostr 設定経路の watch CRUD（載せ替え工程 5-a / Q-A・Q-B）。
//!
//! watch は nostr- 系セッション限定。`interval_secs` は必須（既定値を発明しない）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use opencrab_nostr::NOSTR_SESSION_PREFIX;

#[derive(Debug, Deserialize)]
pub struct WatchWriteBody {
    pub session_id: String,
    pub interval_secs: Option<i64>,
    #[serde(default)]
    pub filter: Option<serde_json::Value>,
}

fn require_nostr_session(session_id: &str) -> Result<(), (StatusCode, String)> {
    if session_id.starts_with(NOSTR_SESSION_PREFIX) && session_id.len() > NOSTR_SESSION_PREFIX.len()
    {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "watch の session_id は nostr- 系に限る".to_string(),
        ))
    }
}

fn require_interval(interval_secs: Option<i64>) -> Result<i64, (StatusCode, String)> {
    match interval_secs {
        None => Err((
            StatusCode::BAD_REQUEST,
            "interval_secs は必須（既定値は無い）".to_string(),
        )),
        Some(n) if n <= 0 => Err((
            StatusCode::BAD_REQUEST,
            "interval_secs は正の整数が必須".to_string(),
        )),
        Some(n) => Ok(n),
    }
}

fn require_filter_json(filter: Option<serde_json::Value>) -> Result<String, (StatusCode, String)> {
    let Some(value) = filter else {
        return Err((
            StatusCode::BAD_REQUEST,
            "filter は必須（空 object は上乗せなし）".to_string(),
        ));
    };
    if !value.is_object() {
        return Err((
            StatusCode::BAD_REQUEST,
            "filter は JSON object が必須".to_string(),
        ));
    }
    Ok(value.to_string())
}

fn watch_json(
    row: &opencrab_db::queries::SessionWatchRow,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let filter: serde_json::Value = serde_json::from_str(&row.filter_json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("filter_json が読めない: {e}"),
        )
    })?;
    Ok(json!({
        "id": row.id,
        "session_id": row.session_id,
        "agent_id": row.agent_id,
        "interval_secs": row.interval_secs,
        "filter": filter,
        "created_at": row.created_at,
    }))
}

/// GET /api/agents/{id}/nostr/watches
pub async fn list_session_watches(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = state.db.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "db lock poisoned".to_string(),
        )
    })?;
    let rows = opencrab_db::queries::list_session_watches_for_agent(&conn, &id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("session_watches を読めない: {e:#}"),
        )
    })?;
    let watches = rows.iter().map(watch_json).collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "watches": watches })))
}

/// POST /api/agents/{id}/nostr/watches
pub async fn create_session_watch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<WatchWriteBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    require_nostr_session(&body.session_id)?;
    let interval = require_interval(body.interval_secs)?;
    let filter_json = require_filter_json(body.filter)?;
    let conn = state.db.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "db lock poisoned".to_string(),
        )
    })?;
    let watch_id = opencrab_db::queries::insert_session_watch(
        &conn,
        &body.session_id,
        &id,
        interval,
        &filter_json,
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let row = opencrab_db::queries::get_session_watch(&conn, watch_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "inserted watch が読めない".to_string(),
            )
        })?;
    Ok(Json(json!({ "created": true, "watch": watch_json(&row)? })))
}

/// PUT /api/agents/{id}/nostr/watches/{watch_id}
pub async fn update_session_watch(
    State(state): State<AppState>,
    Path((id, watch_id)): Path<(String, i64)>,
    Json(body): Json<WatchWriteBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    require_nostr_session(&body.session_id)?;
    let interval = require_interval(body.interval_secs)?;
    let filter_json = require_filter_json(body.filter)?;
    let conn = state.db.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "db lock poisoned".to_string(),
        )
    })?;
    let existing = opencrab_db::queries::get_session_watch(&conn, watch_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "watch が無い".to_string()))?;
    if existing.agent_id != id {
        return Err((
            StatusCode::NOT_FOUND,
            "この agent の watch ではない".to_string(),
        ));
    }
    let ok = opencrab_db::queries::update_session_watch(
        &conn,
        watch_id,
        &body.session_id,
        &id,
        interval,
        &filter_json,
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if !ok {
        return Err((StatusCode::NOT_FOUND, "watch が無い".to_string()));
    }
    let row = opencrab_db::queries::get_session_watch(&conn, watch_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "watch が無い".to_string()))?;
    Ok(Json(json!({ "updated": true, "watch": watch_json(&row)? })))
}

/// DELETE /api/agents/{id}/nostr/watches/{watch_id}
pub async fn delete_session_watch(
    State(state): State<AppState>,
    Path((id, watch_id)): Path<(String, i64)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let conn = state.db.lock().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "db lock poisoned".to_string(),
        )
    })?;
    let existing = opencrab_db::queries::get_session_watch(&conn, watch_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "watch が無い".to_string()))?;
    if existing.agent_id != id {
        return Err((
            StatusCode::NOT_FOUND,
            "この agent の watch ではない".to_string(),
        ));
    }
    let deleted = opencrab_db::queries::delete_session_watch(&conn, watch_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(json!({ "deleted": deleted })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_app_state;

    #[tokio::test]
    async fn create_requires_interval_and_nostr_session() {
        let state = test_app_state();
        let missing = create_session_watch(
            State(state.clone()),
            Path("agent-w".into()),
            Json(WatchWriteBody {
                session_id: "nostr-agent-w".into(),
                interval_secs: None,
                filter: Some(json!({})),
            }),
        )
        .await
        .expect_err("interval 未指定はエラー");
        assert_eq!(missing.0, StatusCode::BAD_REQUEST);
        assert!(missing.1.contains("interval_secs"));

        let discord = create_session_watch(
            State(state.clone()),
            Path("agent-w".into()),
            Json(WatchWriteBody {
                session_id: "discord-a-g-c".into(),
                interval_secs: Some(60),
                filter: Some(json!({})),
            }),
        )
        .await
        .expect_err("discord セッションは拒否");
        assert_eq!(discord.0, StatusCode::BAD_REQUEST);

        let created = create_session_watch(
            State(state.clone()),
            Path("agent-w".into()),
            Json(WatchWriteBody {
                session_id: "nostr-agent-w".into(),
                interval_secs: Some(120),
                filter: Some(json!({"keywords":["x"]})),
            }),
        )
        .await
        .expect("create");
        assert_eq!(created["created"], true);
        assert_eq!(created["watch"]["interval_secs"], 120);

        let listed = list_session_watches(State(state), Path("agent-w".into()))
            .await
            .expect("list");
        assert_eq!(listed["watches"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_and_delete_round_trip() {
        let state = test_app_state();
        let created = create_session_watch(
            State(state.clone()),
            Path("agent-w2".into()),
            Json(WatchWriteBody {
                session_id: "nostr-agent-w2".into(),
                interval_secs: Some(30),
                filter: Some(json!({})),
            }),
        )
        .await
        .unwrap();
        let watch_id = created["watch"]["id"].as_i64().unwrap();
        let updated = update_session_watch(
            State(state.clone()),
            Path(("agent-w2".into(), watch_id)),
            Json(WatchWriteBody {
                session_id: "nostr-agent-w2".into(),
                interval_secs: Some(300),
                filter: Some(json!({"authors":["npub1x"]})),
            }),
        )
        .await
        .unwrap();
        assert_eq!(updated["watch"]["interval_secs"], 300);
        let deleted =
            delete_session_watch(State(state.clone()), Path(("agent-w2".into(), watch_id)))
                .await
                .unwrap();
        assert_eq!(deleted["deleted"], true);
        let listed = list_session_watches(State(state), Path("agent-w2".into()))
            .await
            .unwrap();
        assert!(listed["watches"].as_array().unwrap().is_empty());
    }
}
