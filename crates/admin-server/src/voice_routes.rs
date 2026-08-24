//! D25 `GET|PUT|DELETE /api/voice/config`。正本は `voice_config_override`（B 単独の家）。
//! 秘密は env 名参照のまま JSON にキーを出さない。in-process hot-swap は無い。

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde_json::{json, Value};

use opencrab_db::queries;
use opencrab_voice::{build_stt, build_tts, VoiceConfig};

use crate::api::{AdminState, ApiResult};

fn db_lock(st: &AdminState) -> Result<opencrab_db::DbGuard<'_>, (StatusCode, Json<Value>)> {
    st.db.lock().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "db_lock_error", "detail": e.to_string() })),
        )
    })
}

fn db_err(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    let msg = e.to_string();
    if msg.contains("no such table") || msg.contains("no such column") {
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "unimplemented",
                "detail": format!("正本スキーマ（本体 DB）へ未移行です（migration 待ち）: {msg}"),
            })),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "db_error", "detail": msg })),
        )
    }
}

fn toml_config() -> VoiceConfig {
    VoiceConfig::default()
}

fn runtime_active(st: &AdminState, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    st.store
        .gate_status()
        .ok()
        .map(|rows| {
            rows.iter().any(|row| {
                row.kind_id.as_str() == "discord"
                    && row.connection_state.as_deref() == Some("active")
            })
        })
        .unwrap_or(false)
}

fn envelope(config: VoiceConfig, source: &str, runtime_active: bool) -> Value {
    json!({
        "config": config,
        "source": source,
        "runtime_active": runtime_active,
    })
}

async fn get_voice_config(State(st): State<AdminState>) -> ApiResult<Json<Value>> {
    let raw = {
        let conn = db_lock(&st)?;
        queries::get_voice_config_override(&conn).map_err(db_err)?
    };
    let (config, source) = match raw {
        Some(json_str) => {
            let config: VoiceConfig = serde_json::from_str(&json_str).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "voice_config_override_invalid",
                        "detail": e.to_string(),
                    })),
                )
            })?;
            (config, "db")
        }
        None => (toml_config(), "toml"),
    };
    let active = runtime_active(&st, config.enabled);
    Ok(Json(envelope(config, source, active)))
}

async fn put_voice_config(
    State(st): State<AdminState>,
    Json(config): Json<VoiceConfig>,
) -> ApiResult<Json<Value>> {
    if config.enabled {
        let mut errs = Vec::new();
        if let Err(e) = build_stt(&config.stt) {
            errs.push(format!("STT: {e}"));
        }
        if let Err(e) = build_tts(&config.tts) {
            errs.push(format!("TTS: {e}"));
        }
        if !errs.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_request", "detail": errs.join(" / ") })),
            ));
        }
    }
    let json_str = serde_json::to_string(&config).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "encode_error", "detail": e.to_string() })),
        )
    })?;
    {
        let conn = db_lock(&st)?;
        queries::set_voice_config_override(&conn, &json_str).map_err(db_err)?;
    }
    Ok(Json(json!({
        "saved": true,
        "applied_live": false,
        "restart_required": true,
    })))
}

async fn delete_voice_config(State(st): State<AdminState>) -> ApiResult<Json<Value>> {
    {
        let conn = db_lock(&st)?;
        queries::delete_voice_config_override(&conn).map_err(db_err)?;
    }
    Ok(Json(json!({ "deleted": true, "restart_required": true })))
}

pub fn voice_config_routes() -> Router<AdminState> {
    Router::new().route(
        "/api/voice/config",
        get(get_voice_config)
            .put(put_voice_config)
            .delete(delete_voice_config),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_db::Db;
    use opencrab_store::Store;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn state() -> AdminState {
        AdminState {
            store: Arc::new(Store::new_in_memory().expect("store")),
            db: Arc::new(Db::memory().expect("db")),
            compaction_ratio: 0.5,
        }
    }

    async fn call(st: AdminState, req: Request<Body>) -> (StatusCode, Value) {
        let app = voice_config_routes().with_state(st);
        let res = app.oneshot(req).await.expect("response");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
        (status, body)
    }

    #[tokio::test]
    async fn get_without_override_is_toml_default() {
        let (status, body) = call(
            state(),
            Request::builder()
                .uri("/api/voice/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source"], "toml");
        assert_eq!(body["config"]["enabled"], false);
        assert_eq!(body["runtime_active"], false);
        assert!(body["config"]["stt"]["api_key_env"].is_string());
        assert!(body["config"]["stt"].get("api_key").is_none());
    }

    #[tokio::test]
    async fn put_validates_then_replaces_and_requires_restart() {
        let st = state();
        let cfg = json!({
            "enabled": true,
            "stt": {
                "provider": "openai",
                "model": "whisper-1",
                "api_key_env": "OPENAI_API_KEY"
            },
            "tts": {
                "provider": "voicevox",
                "model": "gpt-4o-mini-tts",
                "api_key_env": "OPENAI_API_KEY",
                "default_voice": "3"
            }
        });
        let (status, body) = call(
            st.clone(),
            Request::builder()
                .method("PUT")
                .uri("/api/voice/config")
                .header("content-type", "application/json")
                .body(Body::from(cfg.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["saved"], true);
        assert_eq!(body["applied_live"], false);
        assert_eq!(body["restart_required"], true);
        let (status, body) = call(
            st,
            Request::builder()
                .uri("/api/voice/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source"], "db");
        assert_eq!(body["config"]["enabled"], true);
        assert_eq!(body["config"]["tts"]["provider"], "voicevox");
    }

    #[tokio::test]
    async fn put_unknown_provider_is_400_and_does_not_write() {
        let st = state();
        let (status, body) = call(
            st.clone(),
            Request::builder()
                .method("PUT")
                .uri("/api/voice/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"enabled": true, "stt": {"provider": "nope"}, "tts": {"provider": "voicevox"}}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = call(
            st,
            Request::builder()
                .uri("/api/voice/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source"], "toml");
    }

    #[tokio::test]
    async fn delete_returns_to_toml() {
        let st = state();
        let _ = call(
            st.clone(),
            Request::builder()
                .method("PUT")
                .uri("/api/voice/config")
                .header("content-type", "application/json")
                .body(Body::from(json!({"enabled": false}).to_string()))
                .unwrap(),
        )
        .await;
        let (status, body) = call(
            st.clone(),
            Request::builder()
                .method("DELETE")
                .uri("/api/voice/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], true);
        assert_eq!(body["restart_required"], true);
        let (status, body) = call(
            st,
            Request::builder()
                .uri("/api/voice/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["source"], "toml");
    }

    #[tokio::test]
    async fn get_without_override_table_is_honest_501() {
        let db = Arc::new(Db::from_connection(
            rusqlite::Connection::open_in_memory().expect("empty"),
        ));
        let st = AdminState {
            store: Arc::new(Store::new_in_memory().expect("store")),
            db,
            compaction_ratio: 0.5,
        };
        let (status, body) = call(
            st,
            Request::builder()
                .uri("/api/voice/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
    }
}
