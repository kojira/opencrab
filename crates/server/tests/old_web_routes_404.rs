//! 旧 Web 会話 5 route は feature 組合せに関係なく 404。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn state() -> opencrab_server::AppState {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    opencrab_server::AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(opencrab_llm::router::LlmRouter::new()),
        llm_config: std::sync::Arc::new(toml::from_str("default_provider = \"\"").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: std::sync::Arc::new(Default::default()),
        voice_runtime: std::sync::Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:test".to_string(),
        tools_config: std::sync::Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
        memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
        memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
        memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        intake: std::sync::Arc::new(Default::default()),
        intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        gateways: std::sync::Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
        session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
        subtask_notifiers: std::sync::Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: std::sync::Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: Default::default(),
        scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        heartbeat_config_rx: opencrab_server::disconnected_heartbeat_config_rx(Default::default()),
        timed_fire_router: std::sync::Arc::new(opencrab_actions::TimedFireRouter::new()),
        progress_debounce: std::sync::Arc::new(
            opencrab_server::subtask_registries::ProgressDebounce::new(),
        ),
    }
}

async fn status(method: &str, uri: &str, body: Option<&'static str>) -> StatusCode {
    let app = opencrab_server::create_router(state());
    let builder = Request::builder().method(method).uri(uri);
    let req = if let Some(b) = body {
        builder
            .header("content-type", "application/json")
            .body(Body::from(b))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let resp = app.oneshot(req).await.unwrap();
    let code = resp.status();
    let _ = resp.into_body().collect().await;
    code
}

#[tokio::test]
async fn old_web_send_is_404() {
    assert_eq!(
        status("POST", "/api/agents/a1/web/send", Some(r#"{"text":"hi"}"#)).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn old_web_stream_is_404() {
    assert_eq!(
        status("GET", "/api/agents/a1/web/stream?conversation=c", None).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn old_session_owner_is_404() {
    assert_eq!(
        status(
            "POST",
            "/api/sessions/web-a1-c/owner",
            Some(r#"{"content":"hi"}"#)
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn old_session_messages_is_404() {
    assert_eq!(
        status(
            "POST",
            "/api/sessions/web-a1-c/messages",
            Some(r#"{"agent_id":"a1","content":"hi"}"#)
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn old_session_mentor_is_404() {
    assert_eq!(
        status(
            "POST",
            "/api/sessions/web-a1-c/mentor",
            Some(r#"{"content":"hi"}"#)
        )
        .await,
        StatusCode::NOT_FOUND
    );
}
