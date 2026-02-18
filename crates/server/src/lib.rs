use std::sync::{Arc, Mutex};

use axum::{
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub mod api;
pub mod config;
pub mod llm_adapter;
pub mod process;

#[cfg(feature = "discord")]
pub mod discord;

use opencrab_llm::router::LlmRouter;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub llm_router: Arc<LlmRouter>,
    pub workspace_base: String,
    pub default_model: String,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        // エージェント管理
        .route("/api/agents", get(api::agents::list_agents).post(api::agents::create_agent))
        .route("/api/agents/{id}", get(api::agents::get_agent).delete(api::agents::delete_agent))
        .route("/api/agents/{id}/soul", get(api::agents::get_soul).put(api::agents::update_soul))
        .route("/api/agents/{id}/identity", get(api::agents::get_identity).put(api::agents::update_identity))
        // スキル管理
        .route("/api/agents/{id}/skills", get(api::skills::list_skills).post(api::skills::add_skill))
        .route("/api/agents/{id}/skills/{skill_id}/toggle", post(api::skills::toggle_skill))
        // 記憶管理
        .route("/api/agents/{id}/memory/curated", get(api::memory::list_curated_memory))
        .route("/api/agents/{id}/memory/search", post(api::memory::search_memory))
        // セッション管理
        .route("/api/sessions", get(api::sessions::list_sessions).post(api::sessions::create_session))
        .route("/api/sessions/{id}", get(api::sessions::get_session))
        .route("/api/sessions/{id}/messages", post(api::sessions::send_message))
        // ワークスペース管理
        .route("/api/agents/{id}/workspace", get(api::workspace::list_workspace))
        .route("/api/agents/{id}/workspace/{*path}", get(api::workspace::read_file).put(api::workspace::write_file))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health_check() -> &'static str {
    "ok"
}
