//! Memory writes（DESIGN-DASHBOARD-P2 SLICE 5）。
//! curated DELETE = `forget`。index / daily-log-index は B 表コマンド。

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use opencrab_store::MemoryIndexError;

use crate::api::{resolve_legacy_agent_id, unresolved_agent, AdminState, ApiResult};

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

fn parse_entry(id: &str) -> ApiResult<i64> {
    id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_id",
                "detail": "entry_id は整数（memories.id）である必要があります",
            })),
        )
    })
}

fn store_err(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
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
            Json(json!({ "error": "store_error", "detail": msg })),
        )
    }
}

fn index_err(error: MemoryIndexError) -> (StatusCode, Json<Value>) {
    match error {
        MemoryIndexError::LlmRequired(detail) => (
            StatusCode::OK,
            Json(json!({ "ok": false, "error": detail })),
        ),
        MemoryIndexError::Store(error) => store_err(error),
    }
}

fn resolve_agent(st: &AdminState, id: &str) -> ApiResult<String> {
    let sid = parse_agent(id)?;
    resolve_legacy_agent_id(st, sid)?.ok_or_else(unresolved_agent)
}

#[derive(Debug, Deserialize)]
struct UpdateMemoryIndexConfigRequest {
    pub batch_size: Option<i64>,
    pub threshold: Option<i64>,
}

async fn delete_curated_memory_entry(
    State(st): State<AdminState>,
    Path((id, entry_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    let entry = parse_entry(&entry_id)?;
    match st.store.forget(agent, entry) {
        Ok(true) => Ok(Json(json!({ "deleted": true }))),
        Ok(false) => Ok(Json(json!({ "deleted": false, "error": "Not found" }))),
        Err(error) => Err(store_err(error)),
    }
}

async fn trigger_memory_index_build(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st.store.memory_index_build(&agent) {
        Ok(result) => Ok(Json(json!({
            "ok": true,
            "nodes_created": result.nodes_created,
            "logs_indexed": result.logs_indexed,
        }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn delete_memory_index(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st.store.memory_index_clear(&agent) {
        Ok(()) => Ok(Json(json!({
            "ok": true,
            "message": "Index deleted",
        }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn update_memory_index_config(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMemoryIndexConfigRequest>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st
        .store
        .memory_index_policy_update(&agent, req.batch_size, req.threshold, now_ns())
    {
        Ok(config) => Ok(Json(json!({
            "ok": true,
            "config": {
                "agent_id": config.agent_id,
                "batch_size": config.batch_size,
                "threshold": config.threshold,
                "updated_at": config.updated_at,
            }
        }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn rebuild_memory_index(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st.store.memory_index_rebuild(&agent) {
        Ok(result) => Ok(Json(json!({
            "ok": true,
            "nodes_created": result.nodes_created,
            "logs_indexed": result.logs_indexed,
        }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn merge_memory_index_topics(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st.store.memory_index_merge(&agent) {
        Ok(result) => Ok(Json(json!({
            "ok": true,
            "periods_processed": result.periods_processed,
            "topics_merged": result.topics_merged,
            "topics_deleted": result.topics_deleted,
        }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn daily_log_rebuild(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st.store.daily_log_index_rebuild(&agent) {
        Ok(()) => Ok(Json(json!({ "status": "started" }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn daily_log_run(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = resolve_agent(&st, &id)?;
    match st.store.daily_log_index_run(&agent) {
        Ok(()) => Ok(Json(json!({ "status": "started" }))),
        Err(error) => Err(index_err(error)),
    }
}

async fn memory_index_status_unimpl() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "unimplemented",
            "detail": "memory index status: unrestored subroute",
        })),
    )
}

pub fn memory_write_routes() -> Router<AdminState> {
    Router::new()
        .route(
            "/api/agents/{id}/memory/curated/{entry_id}",
            delete(delete_curated_memory_entry),
        )
        .route(
            "/api/agents/{id}/memory/index",
            axum::routing::get(memory_index_status_unimpl)
                .post(trigger_memory_index_build)
                .delete(delete_memory_index),
        )
        .route(
            "/api/agents/{id}/memory/index/config",
            put(update_memory_index_config),
        )
        .route(
            "/api/agents/{id}/memory/index/rebuild",
            post(rebuild_memory_index),
        )
        .route(
            "/api/agents/{id}/memory/index/merge",
            post(merge_memory_index_topics),
        )
        .route(
            "/api/agents/{id}/daily-log-index/rebuild",
            post(daily_log_rebuild),
        )
        .route("/api/agents/{id}/daily-log-index/run", post(daily_log_run))
}

#[cfg(test)]
mod contract {
    use super::*;
    use crate::api::{create_router, AdminState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_db::Db;
    use opencrab_store::Store;
    use std::sync::Arc;
    use tower::ServiceExt;

    const LEGACY_UUID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn dummy_db() -> Arc<Db> {
        Arc::new(Db::from_connection(
            rusqlite::Connection::open_in_memory().expect("memory db"),
        ))
    }

    fn body_db_with_agent(name: &str, uuid: &str) -> Arc<Db> {
        let conn = rusqlite::Connection::open_in_memory().expect("memory db");
        conn.execute_batch(&format!(
            "CREATE TABLE agents (agent_id TEXT PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO agents(agent_id, name) VALUES ('{uuid}', '{name}');"
        ))
        .expect("agents");
        Arc::new(Db::from_connection(conn))
    }

    fn store_with_b_tables() -> Store {
        let uri = format!(
            "file:opencrab-mem-b-{}-{}?mode=memory&cache=shared",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let store = Store::open(&uri).expect("store");
        rusqlite::Connection::open(&uri)
            .expect("side")
            .execute_batch(
                "CREATE TABLE memory_index_nodes (
                   id TEXT PRIMARY KEY,
                   agent_id TEXT NOT NULL,
                   parent_id TEXT,
                   node_type TEXT NOT NULL,
                   title TEXT NOT NULL,
                   summary TEXT NOT NULL,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 CREATE TABLE memory_index_watermark (
                   agent_id TEXT PRIMARY KEY,
                   last_indexed_log_id INTEGER NOT NULL DEFAULT 0,
                   last_indexed_at TEXT NOT NULL,
                   total_nodes INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE daily_log_index_watermark (
                   agent_id TEXT NOT NULL PRIMARY KEY,
                   last_indexed_date TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 CREATE TABLE agent_memory_index_config (
                   agent_id TEXT PRIMARY KEY,
                   batch_size INTEGER NOT NULL DEFAULT 50,
                   threshold INTEGER NOT NULL DEFAULT 20,
                   updated_at TEXT NOT NULL
                 );",
            )
            .expect("b tables");
        store
    }

    fn state_from_store(store: Store) -> AdminState {
        AdminState {
            store: Arc::new(store),
            db: dummy_db(),
            compaction_ratio: 0.5,
        }
    }

    fn state_resolved(store: Store) -> AdminState {
        AdminState {
            store: Arc::new(store),
            db: body_db_with_agent("Ada", LEGACY_UUID),
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
    async fn curated_delete_is_forget() {
        let store = Store::new_in_memory().expect("store");
        let (_, created) = call(
            state_from_store(store.clone()),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        let id: i64 = created["id"].as_str().unwrap().parse().unwrap();
        let mid = store.remember(id, "keep this", 0, 0, 0, 5).unwrap();
        let state = state_from_store(store);
        let (status, body) = call(
            state.clone(),
            "DELETE",
            &format!("/api/agents/{id}/memory/curated/{mid}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"deleted": true}));
        let (status, body) = call(
            state,
            "DELETE",
            &format!("/api/agents/{id}/memory/curated/{mid}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"deleted": false, "error": "Not found"}));
    }

    async fn create_ada(state: AdminState) -> (AdminState, String) {
        let (status, created) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        (state, created["id"].as_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn memory_index_writes_land_under_legacy_uuid() {
        let (state, id) = create_ada(state_resolved(store_with_b_tables())).await;

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{id}/memory/index/config"),
            Some(json!({"batch_size": 80})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["config"]["agent_id"], LEGACY_UUID);
        assert_ne!(body["config"]["agent_id"], id);
        assert_eq!(body["config"]["batch_size"], 80);
        assert_eq!(body["config"]["threshold"], 20);

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/memory/index"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"ok": true, "nodes_created": 0, "logs_indexed": 0})
        );

        let (status, body) = call(
            state.clone(),
            "DELETE",
            &format!("/api/agents/{id}/memory/index"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["message"], "Index deleted");

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/memory/index/rebuild"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/memory/index/merge"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["topics_merged"], 0);

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/daily-log-index/run"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"status": "started"}));

        let (status, body) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{id}/memory/index"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        let (status, body) = call(
            state,
            "GET",
            &format!("/api/agents/{id}/memory/index/tree"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    }

    #[tokio::test]
    async fn memory_index_absent_tables_are_501() {
        let (state, id) = create_ada(state_resolved(Store::new_in_memory().expect("store"))).await;
        for (method, uri, body) in [
            (
                "PUT",
                format!("/api/agents/{id}/memory/index/config"),
                Some(json!({"batch_size": 80})),
            ),
            ("POST", format!("/api/agents/{id}/memory/index"), None),
            ("DELETE", format!("/api/agents/{id}/memory/index"), None),
            (
                "POST",
                format!("/api/agents/{id}/memory/index/rebuild"),
                None,
            ),
            ("POST", format!("/api/agents/{id}/memory/index/merge"), None),
            (
                "POST",
                format!("/api/agents/{id}/daily-log-index/run"),
                None,
            ),
            (
                "POST",
                format!("/api/agents/{id}/daily-log-index/rebuild"),
                None,
            ),
        ] {
            let (status, body) = call(state.clone(), method, &uri, body).await;
            assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{method} {uri} {body}");
            assert_eq!(body["error"], "unimplemented");
        }
    }

    #[tokio::test]
    async fn memory_index_unresolved_agent_is_501() {
        let (state, id) =
            create_ada(state_from_store(Store::new_in_memory().expect("store"))).await;
        let (status, body) = call(
            state,
            "PUT",
            &format!("/api/agents/{id}/memory/index/config"),
            Some(json!({"batch_size": 80})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
        assert!(
            body["detail"].as_str().unwrap().contains("旧 agent_id"),
            "{body}"
        );
    }
}
