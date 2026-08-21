use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use opencrab_llm::message::*;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;
use opencrab_server::{create_router, AppState};

/// Create test app using the REAL server router (same as production).
fn create_test_app() -> Router {
    create_test_app_with_db().0
}

/// `create_test_app` と同じアプリを作り、同じ DB ハンドルも返す。
///
/// API を経由せずに行を書き込みたいテスト（入口の正規化が入る前に保存された
/// レガシー行の再現）で使う。
fn create_test_app_with_db() -> (Router, opencrab_db::Db) {
    let (state, db) = create_test_state(0.5);
    (create_router(state), db)
}

/// 既定ヘルパと同じ `AppState` を組むが、`compaction_ratio` だけ引数で差し替える。
/// compaction_ratio を state から読んでいる経路（例: model_pricing 一覧 API）を
/// 恒真にならない値で検証するために使う。
fn create_test_state(compaction_ratio: f64) -> (AppState, opencrab_db::Db) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let state = AppState {
        db: db.clone(),
        llm_router: opencrab_server::SharedLlmRouter::new(LlmRouter::new()),
        // 既定を明示的に置かない（default_provider = ""）。テストの base は provider 未定義で、
        // provider を DB オーバーライドで足す reload 経路（build_llm_router）が、コード既定の
        // sentinel "openai" と食い違って default_provider 検証で弾かれるのを避ける（#660）。
        // 空 default_provider は「既定なし＝未設定」として検証をスキップする正規の状態。
        llm_config: Arc::new(toml::from_str("default_provider = \"\"").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:test".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio,
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
        #[cfg(feature = "web")]
        web_gateway: std::sync::Arc::new(opencrab_web_gateway::WebGateway::new()),
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
    };
    (state, db)
}

// ==================== Helper ====================

async fn send_request(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let body = match body {
        Some(json) => Body::from(serde_json::to_vec(&json).unwrap()),
        None => Body::empty(),
    };

    let mut builder = Request::builder().uri(uri);
    builder = match method {
        "GET" => builder.method("GET"),
        "POST" => builder.method("POST"),
        "PUT" => builder.method("PUT"),
        "PATCH" => builder.method("PATCH"),
        "DELETE" => builder.method("DELETE"),
        _ => panic!("unsupported method"),
    };

    if method == "POST" || method == "PUT" || method == "PATCH" {
        builder = builder.header("content-type", "application/json");
    }

    let req = builder.body(body).unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!(body_bytes.to_vec()));
    (status, json)
}

/// Create an agent via API and return its ID.
async fn create_test_agent(app: Router) -> (String, Router) {
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": "Test Agent",
            "persona_name": "TestPersona"
        })),
    )
    .await;
    let agent_id = resp["id"].as_str().unwrap().to_string();
    (agent_id, app)
}

// ==================== Tests ====================

#[tokio::test]
async fn test_health_check() {
    let app = create_test_app();
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn test_model_pricing_list_exposes_compaction_ratio() {
    // 実効予算 = context_window × compaction_ratio をフロントが計算するには
    // compaction_ratio が要る（#484）。ここでは **既定 0.5 を避けて** 0.375 を state に
    // 入れ、ハンドラが state.compaction_ratio を読んでいる（定数を返していない）ことを
    // 確かめる。行も 1 件入れて models と同居することを見る。
    let (state, db) = create_test_state(0.375);
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "chatgpt".to_string(),
                model: "gpt-5.6-luna".to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(400_000),
                max_output_tokens: None,
            },
        )
        .unwrap();
    }
    let app = create_router(state);

    let (status, body) = send_request(app, "GET", "/api/llm/model-pricing", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["compaction_ratio"].as_f64(), Some(0.375));
    let models = body["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["context_window"].as_i64(), Some(400_000));
}

#[tokio::test]
async fn test_create_and_get_agent() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["name"], "Test Agent");
    assert_eq!(resp["persona_name"], "TestPersona");
}

#[tokio::test]
async fn test_list_agents() {
    let app = create_test_app();
    let (_, app) = create_test_agent(app).await;
    let (_, app) = create_test_agent(app).await;

    let (status, resp) = send_request(app, "GET", "/api/agents", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn test_delete_agent() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(
        app.clone(),
        "DELETE",
        &format!("/api/agents/{agent_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["deleted"], true);

    // Verify gone
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp.is_null());
}

#[tokio::test]
async fn test_patch_agent_persona() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "persona_name": "UpdatedPersona",
            "personality": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["persona_name"], "UpdatedPersona");
}

#[tokio::test]
async fn test_patch_agent_identity_fields() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Updated Name",
            "job_title": "Lead",
            "organization": "OpenCrab Inc",
            "image_url": null,
            "metadata_json": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["name"], "Updated Name");
}

#[tokio::test]
async fn test_create_and_list_sessions() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Test Discussion",
            "participant_ids": [agent_id]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["id"].as_str().is_some());

    let (_, resp) = send_request(app, "GET", "/api/sessions", None).await;
    assert!(!resp.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_get_session() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Session Theme",
            "participant_ids": [agent_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    let (status, resp) =
        send_request(app, "GET", &format!("/api/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["theme"], "Session Theme");
}

#[tokio::test]
async fn test_send_message_to_session() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Messaging Test",
            "participant_ids": [&agent_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "content": "Hello world"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["id"].as_i64().is_some());
    assert_eq!(resp["session_id"], session_id);
}

#[tokio::test]
async fn test_add_and_list_skills() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills"),
        Some(serde_json::json!({
            "name": "Test Skill",
            "description": "A test skill",
            "situation_pattern": "test_pattern",
            "guidance": "Use wisely"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["id"].as_str().is_some());

    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}/skills"), None).await;
    let skills = resp.as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "Test Skill");
}

#[tokio::test]
async fn test_toggle_skill() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills"),
        Some(serde_json::json!({
            "name": "Toggle Skill",
            "description": "desc",
            "situation_pattern": "",
            "guidance": ""
        })),
    )
    .await;
    let skill_id = resp["id"].as_str().unwrap().to_string();

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills/{skill_id}/toggle"),
        Some(serde_json::json!({"active": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["toggled"], true);

    // Verify skill is now inactive
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}/skills"), None).await;
    let skills = resp.as_array().unwrap();
    let skill = skills.iter().find(|s| s["id"] == skill_id).unwrap();
    assert_eq!(skill["is_active"], false);
}

#[tokio::test]
async fn test_list_curated_memory_empty() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/memory/curated"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["items"].as_array().unwrap().len(), 0);
    assert_eq!(resp["total"].as_i64().unwrap(), 0);
}

#[tokio::test]
async fn test_search_memory() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    // Create session and send messages
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Search Test",
            "participant_ids": [&agent_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "content": "Rust programming is fun"
        })),
    )
    .await;

    send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "content": "Python is also great"
        })),
    )
    .await;

    // Search
    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/agents/{agent_id}/memory/search"),
        Some(serde_json::json!({
            "query": "Rust",
            "limit": 10
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["count"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn test_full_workflow() {
    let app = create_test_app();

    // 1. Create agent
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": "Workflow Agent",
            "persona_name": "WorkflowPersona"
        })),
    )
    .await;
    let agent_id = resp["id"].as_str().unwrap().to_string();

    // 2. Create session
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Full Workflow Test",
            "participant_ids": [&agent_id],
            "max_turns": 10
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // 3. Send 3 messages
    for content in &[
        "The architecture of OpenCrab is modular",
        "Each agent has a soul and identity",
        "Skills can be acquired at runtime",
    ] {
        send_request(
            app.clone(),
            "POST",
            &format!("/api/sessions/{session_id}/messages"),
            Some(serde_json::json!({
                "agent_id": agent_id,
                "content": content
            })),
        )
        .await;
    }

    // 4. Search memory
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/memory/search"),
        Some(serde_json::json!({
            "query": "soul",
            "limit": 10
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let count = resp["count"].as_i64().unwrap();
    assert!(count >= 1, "Expected at least 1 search result, got {count}");

    // 5. Verify session state
    let (_, resp) = send_request(
        app.clone(),
        "GET",
        &format!("/api/sessions/{session_id}"),
        None,
    )
    .await;
    assert_eq!(resp["theme"], "Full Workflow Test");

    // 6. Get agent
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["name"], "Workflow Agent");
}

// ── Agent CRUD cycle (mirrors dashboard operations) ──

#[tokio::test]
async fn test_agent_crud_full_cycle() {
    let app = create_test_app();

    // 1. Create
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": "CRUD Agent",
            "persona_name": "CRUD Persona"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let agent_id = resp["id"].as_str().unwrap().to_string();

    // 2. Read - verify created
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["name"], "CRUD Agent");
    assert_eq!(resp["persona_name"], "CRUD Persona");

    // 3–4. Partial updates (旧 identity / soul 相当)
    let (status, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Updated CRUD Agent",
            "job_title": "Team Lead",
            "organization": "OpenCrab Labs",
            "image_url": null,
            "metadata_json": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "persona_name": "Updated CRUD Persona",
            "personality": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 5. Read - verify both updates
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["name"], "Updated CRUD Agent");
    assert_eq!(resp["job_title"], "Team Lead");
    assert_eq!(resp["organization"], "OpenCrab Labs");
    assert_eq!(resp["persona_name"], "Updated CRUD Persona");

    // 6. Verify shows in list
    let (_, resp) = send_request(app.clone(), "GET", "/api/agents", None).await;
    let agents = resp.as_array().unwrap();
    let found = agents.iter().any(|a| a["name"] == "Updated CRUD Agent");
    assert!(found, "Updated agent should appear in list");

    // 7. Delete
    let (status, resp) = send_request(
        app.clone(),
        "DELETE",
        &format!("/api/agents/{agent_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["deleted"], true);

    // 8. Verify gone from list
    let (_, resp) = send_request(app.clone(), "GET", "/api/agents", None).await;
    let agents = resp.as_array().unwrap();
    let found = agents.iter().any(|a| a["id"] == agent_id);
    assert!(!found, "Deleted agent should not appear in list");

    // 9. Verify get returns null
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp.is_null());
}

#[tokio::test]
async fn test_create_agent_minimal_fields() {
    let app = create_test_app();

    // Create with only name and persona_name
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": "Minimal Agent",
            "persona_name": "MinimalPersona"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let agent_id = resp["id"].as_str().unwrap().to_string();

    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["name"], "Minimal Agent");
}

#[tokio::test]
async fn test_delete_nonexistent_agent() {
    let app = create_test_app();

    let (status, resp) =
        send_request(app, "DELETE", "/api/agents/nonexistent-id-12345", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["deleted"], false);
}

// ==================== Caller identity (owner) via handler ====================
//
// `is_owner_id` の単体テストだけでは、実際のハンドラがそれを通っている保証が
// 無い（判定を素朴な `==` に戻しても単体テストは緑のまま）。ここでは
// `POST /api/agents/{id}/messages` を実際に叩いて caller 判定を検証する。
// LLM プロバイダは 0 件なのでハンドラは早期 return し、`caller_type` を JSON で返す。

/// per-agent Discord 設定を保存する（owner を明示指定）。
async fn set_agent_owner(app: Router, agent_id: &str, owner_discord_id: &str) -> Router {
    let (status, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/discord"),
        Some(serde_json::json!({
            "bot_token": "test-token",
            "owner_discord_id": owner_discord_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["ok"], true, "discord config must be saved: {resp}");
    app
}

/// `POST /api/agents/{id}/messages` を叩いて `caller_type` を返す。
async fn caller_type_for(app: Router, agent_id: &str, user_id: &str) -> (Router, String) {
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({
            "content": "hello",
            "user_id": user_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let caller_type = resp["caller_type"]
        .as_str()
        .unwrap_or_else(|| panic!("response must carry caller_type: {resp}"))
        .to_string();
    (app, caller_type)
}

/// owner 未設定（per-agent Discord 設定の owner が空文字）のとき、空の `user_id`
/// で呼んでも Owner 権限にならない。
#[tokio::test]
async fn test_empty_user_id_is_not_owner_when_owner_unset() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, "").await;
    assert_ne!(
        caller_type, "owner",
        "empty user_id must not be promoted to owner when owner is unset"
    );
    assert_eq!(caller_type, "agent");
}

/// 空白のみの owner を保存しても owner は「未設定」のままで、空白のみの `user_id`
/// で呼んでも Owner 権限にならない。
///
/// PUT の入口 trim により `" "` は `""` として保存されるため、これは「空 owner」の
/// 検証になる（空白のまま保存された**レガシー行**の経路は
/// `test_legacy_whitespace_only_owner_row_matches_nobody` が受け持つ）。
#[tokio::test]
async fn test_whitespace_user_id_is_not_owner_when_owner_blank() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, " ").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, " ").await;
    assert_ne!(
        caller_type, "owner",
        "whitespace-only owner must be treated as unset"
    );
    assert_eq!(caller_type, "agent");
}

/// 正のコントロール: owner を設定すれば、その `user_id` はハンドラ経路で
/// Owner として認識される（ガードが過剰に効いて owner を落としていない）。
#[tokio::test]
async fn test_configured_owner_is_recognized_through_handler() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, "123456789012345678").await;
    assert_eq!(caller_type, "owner");
}

/// 負のコントロール: owner 設定済みでも、**別の** `user_id` は Owner にならない
/// （ガードが過少に振れて誰でも owner になっていない）。
#[tokio::test]
async fn test_other_user_is_not_owner_when_owner_configured() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, "987654321098765432").await;
    assert_ne!(
        caller_type, "owner",
        "a different user_id must not be recognized as owner"
    );
    assert_eq!(caller_type, "agent");
}

/// `PUT /api/agents/{id}/discord` は owner を trim して保存する。
///
/// 前後空白付きのまま保存すると、trim 済み比較を行う経路（`is_owner_id`）では
/// owner と認識されるのに、生比較が残る下位経路（form/modal）だけ無言で拒否される
/// 半端な状態になる。入口で正規化して防ぐ（判定述語の共通化は #174）。
#[tokio::test]
async fn test_owner_discord_id_is_trimmed_on_save() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "  123456789012345678\n").await;

    // 保存値そのものが trim されている。
    let (status, resp) = send_request(
        app.clone(),
        "GET",
        &format!("/api/agents/{agent_id}/discord"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["owner_discord_id"], "123456789012345678");

    // ハンドラ経路でも trim 済みの ID が owner として通る。
    let (_app, caller_type) = caller_type_for(app, &agent_id, "123456789012345678").await;
    assert_eq!(caller_type, "owner");
}

/// `PATCH /api/agents/{id}/discord` も owner を trim して保存する。
///
/// PUT だけ直しても、ダッシュボードからの部分更新（PATCH）経路から空白付きの owner が
/// 入り込む余地が残る（PUT 版は `test_owner_discord_id_is_trimmed_on_save`）。
#[tokio::test]
async fn test_owner_discord_id_is_trimmed_on_patch() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    // PATCH は設定済みの行にしか効かないので、まず PUT で作る。
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (status, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}/discord"),
        Some(serde_json::json!({
            "owner_discord_id": "  987654321098765432\n",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["ok"], true, "patch must succeed: {resp}");
    assert_eq!(
        resp["owner_discord_id"], "987654321098765432",
        "PATCH は owner を trim して保存する: {resp}"
    );

    // 保存値そのものが trim されている。
    let (status, resp) = send_request(
        app.clone(),
        "GET",
        &format!("/api/agents/{agent_id}/discord"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["owner_discord_id"], "987654321098765432");

    // ハンドラ経路でも新しい owner が通り、置き換えられた古い owner は通らない。
    let (app, caller_type) = caller_type_for(app, &agent_id, "987654321098765432").await;
    assert_eq!(caller_type, "owner");
    let (_app, caller_type) = caller_type_for(app, &agent_id, "123456789012345678").await;
    assert_ne!(
        caller_type, "owner",
        "replaced owner must lose owner rights"
    );
}

/// レガシー行（入口 trim を入れる前に空白付きで保存された `owner_discord_id`）でも
/// ハンドラ経路の owner 判定が成立する。
///
/// 入口の正規化は新規保存にしか効かないので、既存 DB の行は空白付きのまま残る。
/// API を経由せず DB に直接書いてその状態を再現する。
#[tokio::test]
async fn test_legacy_padded_owner_row_is_still_recognized() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    {
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::patch_agent_discord_config(
            &conn,
            &agent_id,
            None,
            Some("  123456789012345678\n"),
        )
        .unwrap());
    }

    let (app, caller_type) = caller_type_for(app, &agent_id, "123456789012345678").await;
    assert_eq!(
        caller_type, "owner",
        "padded legacy owner row must still match the owner"
    );
    // 別 ID は当然 owner ではない（trim 比較が緩みすぎていない）。
    let (_app, caller_type) = caller_type_for(app, &agent_id, "987654321098765432").await;
    assert_ne!(caller_type, "owner");
}

/// レガシー行の owner が空白のみなら「未設定」として扱い、誰も owner に昇格させない。
#[tokio::test]
async fn test_legacy_whitespace_only_owner_row_matches_nobody() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    {
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::patch_agent_discord_config(
            &conn,
            &agent_id,
            None,
            Some(" \t\n")
        )
        .unwrap());
    }

    let mut app = app;
    for user_id in [" \t\n", " ", "", "123456789012345678"] {
        let (next, caller_type) = caller_type_for(app, &agent_id, user_id).await;
        app = next;
        assert_ne!(
            caller_type, "owner",
            "whitespace-only legacy owner must match nobody (user_id={user_id:?})"
        );
    }
}

/// `user_id` の前後空白はハンドラ入口で 1 回だけ正規化され、認可・セッションキー・
/// `speaker_id` すべてで同じ値が使われる（owner にはなれるのに別セッションに
/// 記録される、という非対称を作らない）。
#[tokio::test]
async fn test_user_id_is_trimmed_consistently() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({
            "content": "hello",
            "user_id": "  123456789012345678 ",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["caller_type"], "owner");
    assert_eq!(
        resp["session_id"],
        format!("agent-msg-{agent_id}-123456789012345678"),
        "session id must use the trimmed user_id: {resp}"
    );
}

// ==================== MockLlmProvider ====================

/// A mock LLM provider that returns pre-queued responses.
struct MockLlmProvider {
    responses: Mutex<VecDeque<ChatResponse>>,
    /// 受け取った各リクエストの system prompt（連結）。何ターン目にどんな system prompt で
    /// 呼ばれたかを検証する（subtask 完了 resume の注入マーカーの確認に使う）。
    system_prompts: Mutex<Vec<String>>,
}

impl MockLlmProvider {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            system_prompts: Mutex::new(Vec::new()),
        }
    }

    /// これまでに受け取ったリクエストの system prompt 一覧（呼ばれた順）。
    fn system_prompts(&self) -> Vec<String> {
        self.system_prompts.lock().unwrap().clone()
    }

    fn push_text_response(&self, text: &str) {
        let response = ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(text),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            created: 0,
        };
        self.responses.lock().unwrap().push_back(response);
    }

    fn push_tool_call_response(&self, tool_calls: Vec<ToolCall>) {
        let mut msg = Message {
            role: Role::Assistant,
            content: None,
            name: None,
            function_call: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        };
        let _ = &mut msg; // suppress unused_mut

        let response = ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: msg,
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: Usage::default(),
            created: 0,
        };
        self.responses.lock().unwrap().push_back(response);
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockLlmProvider {
    fn name(&self) -> &str {
        "mock"
    }

    // #676: このモックは max_tokens を無視するので「送らない」を宣言し、run 経路の
    // 出力上限モデル登録（fail loud）の対象外にする。上限の解決/ゲートは context_budget /
    // skill_engine の専用テスト、および明示 register する API ゲートテストで担保する。
    fn sends_max_output_tokens(&self) -> bool {
        false
    }

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        // キューが尽きていても記録は残す（何回どんな system prompt で呼ばれたかが証拠）。
        let system = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .filter_map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        self.system_prompts.lock().unwrap().push(system);

        let mut queue = self.responses.lock().unwrap();
        queue
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("MockLlmProvider: no more queued responses"))
    }
}

// ==================== LLM-integrated helpers ====================

/// Create test app with a MockLlmProvider registered in the LlmRouter.
/// Returns (Router, opencrab_db::Db, Arc<MockLlmProvider>).
fn create_test_app_with_llm() -> (Router, opencrab_db::Db, Arc<MockLlmProvider>) {
    let (app, db, mock, _state) = create_test_app_with_state();
    (app, db, mock)
}

/// `create_test_app_with_llm` の `AppState` も返す版（#169）。
/// dispatch registry のような「ハンドラとテストが共有するランタイム状態」を検証するのに使う。
fn create_test_app_with_state() -> (Router, opencrab_db::Db, Arc<MockLlmProvider>, AppState) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);

    let mock = Arc::new(MockLlmProvider::new());
    let mut router = LlmRouter::new();
    router.add_provider(mock.clone() as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");

    let state = AppState {
        db: db.clone(),
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir()
            .join("opencrab_test")
            .to_string_lossy()
            .to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
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
        #[cfg(feature = "web")]
        web_gateway: std::sync::Arc::new(opencrab_web_gateway::WebGateway::new()),
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
    };
    let app = create_router(state.clone());
    (app, db, mock, state)
}

/// Create a named agent with a specific persona via the API.
async fn create_test_agent_named(app: Router, name: &str, persona: &str) -> (String, Router) {
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": name,
            "persona_name": persona
        })),
    )
    .await;
    let agent_id = resp["id"].as_str().unwrap().to_string();
    (agent_id, app)
}

// ==================== LLM-integrated E2E Tests ====================

/// Test: Agent A sends a message → Agent B responds via SkillEngine.
#[tokio::test]
async fn test_send_message_triggers_agent_response() {
    let (app, _db, mock) = create_test_app_with_llm();

    // Create two agents.
    let (agent_a, app) = create_test_agent_named(app, "Alice", "Curious Researcher").await;
    let (agent_b, app) = create_test_agent_named(app, "Bob", "Creative Thinker").await;

    // Create session with both agents.
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "AI Ethics",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Queue a text response for Bob (when Alice sends a message).
    mock.push_text_response("That's a fascinating point about AI ethics! I think we need to consider both fairness and transparency.");

    // Alice sends a message.
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "What are your thoughts on AI ethics?"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify the response contains Bob's SkillEngine-driven reply.
    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1, "Expected 1 response (from Bob)");
    assert_eq!(responses[0]["agent_id"], agent_b);
    assert!(
        responses[0]["content"]
            .as_str()
            .unwrap()
            .contains("fairness"),
        "Response should contain the mock text"
    );
    assert_eq!(responses[0]["tool_calls_made"], 0);
}

/// Test: Two rounds of discussion, second round agent calls learn_from_experience
/// which creates a skill in the DB.
#[tokio::test]
async fn test_agents_discuss_and_generate_skill() {
    let (app, db, mock) = create_test_app_with_llm();

    // Create two agents.
    let (agent_a, app) = create_test_agent_named(app, "Researcher", "Analytical Mind").await;
    let (agent_b, app) = create_test_agent_named(app, "Creator", "Innovative Spirit").await;

    // Create session.
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Learning from discussions",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Round 1: Agent A sends → Agent B responds with text.
    mock.push_text_response("I've learned a lot from this discussion about knowledge sharing.");

    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "How do you approach knowledge sharing?"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Round 2: Agent B sends → Agent A uses learn_from_experience tool, then responds.
    // Queue: first a tool call response, then a text response after tool execution.
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-learn-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "collaborative_learning",
                "description": "Skill for learning through collaborative discussions",
                "situation_pattern": "when discussing with other agents",
                "guidance": "Ask open-ended questions and synthesize different perspectives"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response(
        "I've just created a new skill called 'collaborative_learning' based on our discussion!",
    );

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_b,
            "content": "Let me reflect on what I learned from you."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1, "Expected 1 response (from Agent A)");

    // Verify the response used tool calls.
    let tool_calls_made = responses[0]["tool_calls_made"].as_i64().unwrap();
    assert_eq!(tool_calls_made, 1, "Agent A should have made 1 tool call");

    // Verify skill was created in the DB.
    let skills = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_skills(&conn, responses[0]["agent_id"].as_str().unwrap(), false)
            .unwrap()
    };
    assert!(
        !skills.is_empty(),
        "Agent A should have a skill in the DB after learn_from_experience"
    );

    let skill = skills.iter().find(|s| s.name == "collaborative_learning");
    assert!(
        skill.is_some(),
        "Should find 'collaborative_learning' skill"
    );
    let skill = skill.unwrap();
    assert_eq!(skill.source_type, "experience");
    assert!(skill.is_active);
}

/// Test: Full LLM cost optimization cycle.
/// Agent A sends → Agent B responds (metrics recorded) →
/// Agent A sends again → Agent B calls analyze_llm_usage → optimize_model_selection → select_llm.
#[tokio::test]
async fn test_llm_optimization_cycle() {
    let (app, db, mock) = create_test_app_with_llm();

    let (agent_a, app) = create_test_agent_named(app, "User", "Curious").await;
    let (agent_b, app) = create_test_agent_named(app, "Optimizer", "Cost-conscious").await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Cost optimization",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Round 1: Simple conversation. Agent B just responds with text.
    // This records metrics to DB via LlmRouterAdapter.
    mock.push_text_response("Hello! Let me think about this topic.");

    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "Let's discuss cost optimization."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify metrics were recorded in DB after round 1.
    {
        let conn = db.lock().unwrap();
        let summary =
            opencrab_db::queries::get_llm_metrics_summary(&conn, &agent_b, "1970-01-01").unwrap();
        assert_eq!(
            summary.count, 1,
            "Should have 1 metrics record after round 1"
        );
    }

    // Round 2: Agent B uses the optimization tools.
    // Step 1: analyze_llm_usage
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-analyze".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "analyze_llm_usage".to_string(),
            arguments: serde_json::json!({"period": "all"}).to_string(),
        },
    }]);
    // Step 2: recall_model_experiences
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-recall".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "recall_model_experiences".to_string(),
            arguments: serde_json::json!({}).to_string(),
        },
    }]);
    // Step 3: select_llm to switch model (using mock provider's model)
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-select".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "select_llm".to_string(),
            arguments: serde_json::json!({
                "model_alias": "mock:fast-model",
                "reason": "Cheaper model is sufficient for this conversation",
                "purpose": "conversation",
            })
            .to_string(),
        },
    }]);
    // Step 4: evaluate_response
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-eval".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "evaluate_response".to_string(),
            arguments: serde_json::json!({
                "quality_score": 0.85,
                "task_success": true,
                "evaluation": "Model selection was effective, switching to cheaper model",
            })
            .to_string(),
        },
    }]);
    // Final text response after all tool calls.
    mock.push_text_response(
        "I've analyzed my usage and switched to a more cost-effective model. The cheaper model should work well for our conversation.",
    );

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "Can you optimize your model usage?"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1);
    let tool_calls_made = responses[0]["tool_calls_made"].as_i64().unwrap();
    assert_eq!(
        tool_calls_made, 4,
        "Expected 4 tool calls: analyze + optimize + select + evaluate"
    );
    assert!(responses[0]["content"]
        .as_str()
        .unwrap()
        .contains("cost-effective"),);

    // Verify metrics were recorded for both rounds.
    {
        let conn = db.lock().unwrap();
        let summary =
            opencrab_db::queries::get_llm_metrics_summary(&conn, &agent_b, "1970-01-01").unwrap();
        // Round 1 = 1 call, Round 2 = 5 calls (analyze→optimize→select→evaluate→final).
        assert!(
            summary.count >= 2,
            "Should have at least 2 metrics records, got {}",
            summary.count
        );
    }
}

/// Test: When no LLM providers are registered, send_message falls back to
/// legacy behavior (just logs, no SkillEngine, backward compatible).
#[tokio::test]
async fn test_send_message_without_llm_falls_back() {
    // Use the standard test app (no LLM providers).
    let app = create_test_app();

    let (agent_a, app) = create_test_agent_named(app, "Solo", "Independent").await;
    let (agent_b, app) = create_test_agent_named(app, "Partner", "Collaborative").await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Fallback Test",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Send message — should return legacy format without "responses".
    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "Hello"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["id"].as_i64().is_some());
    assert_eq!(resp["session_id"], session_id);
    // No "responses" field in legacy mode.
    assert!(resp.get("responses").is_none());
}

// ==================== Import API E2E Tests ====================

#[tokio::test]
async fn test_import_scan_empty_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let app = create_test_app();

    let (status, resp) = send_request(
        app,
        "POST",
        "/api/import/scan",
        Some(serde_json::json!({
            "source_dir": tmp.path().to_str().unwrap(),
            "options": {
                "include_daily_logs": false,
                "daily_log_days": 7,
                "include_skills": false,
                "overwrite_if_exists": false
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["soul"].is_object());
    assert_eq!(resp["soul"]["found"], false);
    assert_eq!(resp["identity"]["found"], false);
}

#[tokio::test]
async fn test_import_scan_with_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("SOUL.md"),
        "# SOUL.md\n## Vibe\nYou are **TestBot**.\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("IDENTITY.md"),
        "# IDENTITY.md\n- **Name:** TestBot\n- **Avatar:** https://example.com/img.png\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("MEMORY.md"),
        "# MEMORY\n## Facts\nSome facts.\n## Rules\nSome rules.\n",
    )
    .unwrap();

    let app = create_test_app();
    let (status, resp) = send_request(
        app,
        "POST",
        "/api/import/scan",
        Some(serde_json::json!({
            "source_dir": tmp.path().to_str().unwrap(),
            "options": {
                "include_daily_logs": false,
                "daily_log_days": 7,
                "include_skills": true,
                "overwrite_if_exists": false
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["soul"]["found"], true);
    assert_eq!(resp["identity"]["found"], true);
    assert_eq!(resp["identity"]["name"], "TestBot");
    assert_eq!(resp["memory_curated"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_import_execute_not_confirmed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let app = create_test_app();

    let (status, resp) = send_request(
        app,
        "POST",
        "/api/import/execute",
        Some(serde_json::json!({
            "source_dir": tmp.path().to_str().unwrap(),
            "agent_name": "Test",
            "options": {
                "include_daily_logs": false,
                "daily_log_days": 7,
                "include_skills": false,
                "overwrite_if_exists": false
            },
            "confirmed": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(resp["error"].as_str().unwrap().contains("confirmed"));
}

#[tokio::test]
async fn test_import_execute_full() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("SOUL.md"),
        "# SOUL.md\n## Vibe\nYou are **ImportBot**.\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("IDENTITY.md"),
        "# IDENTITY.md\n- **Name:** ImportBot\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("MEMORY.md"),
        "# MEMORY\n## Knowledge\nSome knowledge.\n",
    )
    .unwrap();
    let skill_dir = tmp.path().join("skills").join("greet");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# Greeting Skill\nSay hello.\n").unwrap();

    let app = create_test_app();
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        "/api/import/execute",
        Some(serde_json::json!({
            "source_dir": tmp.path().to_str().unwrap(),
            "agent_name": "ImportBot",
            "options": {
                "include_daily_logs": false,
                "daily_log_days": 7,
                "include_skills": true,
                "overwrite_if_exists": true
            },
            "confirmed": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp["agent_id"].as_str().is_some());
    let result = &resp["result"];
    assert_eq!(result["counts"]["soul"], true);
    assert_eq!(result["counts"]["identity"], true);
    assert_eq!(result["counts"]["memory_curated"], 1);
    assert_eq!(result["counts"]["skills"], 1);

    // Verify agent was actually created
    let agent_id = resp["agent_id"].as_str().unwrap();
    let (status, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["name"], "ImportBot");
}

// ==================== Provider Settings (dashboard) ====================

#[tokio::test]
async fn test_list_llm_providers() {
    let app = create_test_app();
    let (status, json) = send_request(app, "GET", "/api/llm/providers", None).await;
    assert_eq!(status, StatusCode::OK);
    let providers = json["providers"].as_array().unwrap();
    // 既知プロバイダーは TOML/DB に無くても列挙される
    let names: Vec<&str> = providers
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"ollama"));
    // 未設定なのでキーは none / 非稼働
    let openai = providers.iter().find(|p| p["name"] == "openai").unwrap();
    assert_eq!(openai["api_key_source"], "none");
    assert_eq!(openai["active"], false);
}

#[tokio::test]
async fn test_update_provider_sets_key_and_reloads() {
    let app = create_test_app();
    // API キーを設定 → ルーター再構築で openai が稼働状態になる
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({"api_key": "sk-test-dashboard-key", "default_model": "gpt-4o"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["reloaded"], true);
    let p = &json["provider"];
    assert_eq!(
        p["active"], true,
        "provider should be live after key set: {p}"
    );
    assert_eq!(p["api_key_source"], "db");
    // 平文キーは応答に含まれない（マスクのみ）
    assert!(!json.to_string().contains("sk-test-dashboard-key"));
    assert_eq!(p["api_key_masked"], "••••-key");

    // オーバーライド削除 → 非稼働に戻る
    let (status, json) = send_request(
        app.clone(),
        "DELETE",
        "/api/llm/providers/openai/override",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (_, json) = send_request(app, "GET", "/api/llm/providers", None).await;
    let openai = json["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "openai")
        .unwrap()
        .clone();
    assert_eq!(openai["active"], false);
    assert_eq!(openai["has_override"], false);
}

#[tokio::test]
async fn test_update_provider_disable_and_reject_bad_name() {
    let app = create_test_app();
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/ollama",
        Some(serde_json::json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["provider"]["enabled_override"], false);

    // 不正なプロバイダー名は 400
    let (status, _) = send_request(
        app,
        "PUT",
        "/api/llm/providers/bad%2Fname",
        Some(serde_json::json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_agent_reasoning_effort_patch_roundtrip() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    // 既定は未設定（null）
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp["reasoning_effort"].is_null());

    // PATCH で設定
    let (status, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({ "reasoning_effort": "high" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["reasoning_effort"], "high");

    // 空文字で解除 → NULL に正規化される（null は serde の都合でクリア不可のため）
    let (_, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({ "reasoning_effort": "" })),
    )
    .await;
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp["reasoning_effort"].is_null());
}

#[tokio::test]
async fn test_codex_diagnostics_returns_fields() {
    let app = create_test_app();
    let (status, json) = send_request(app, "GET", "/api/llm/codex/diagnostics", None).await;
    assert_eq!(status, StatusCode::OK);
    // テスト設定には codex プロバイダーが無いので configured_path は既定の "codex"
    assert_eq!(json["configured_path"], "codex");
    // version/resolved_path/error のキーが存在すること（値は環境依存）
    assert!(json.get("version").is_some());
    assert!(json.get("resolved_path").is_some());
    assert!(json.get("error").is_some());
}

#[tokio::test]
async fn test_update_provider_reasoning_effort_roundtrip() {
    let app = create_test_app();
    // 推論強度を設定
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/codex",
        Some(serde_json::json!({ "reasoning_effort": "medium" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["provider"]["reasoning_effort"], "medium");
    assert_eq!(json["provider"]["has_override"], true);

    // GET でも反映
    let (_, json) = send_request(app.clone(), "GET", "/api/llm/providers", None).await;
    let codex = json["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "codex")
        .unwrap()
        .clone();
    assert_eq!(codex["reasoning_effort"], "medium");

    // null で解除 → モデル既定（空）に戻り、他フィールドが無ければ行ごと消える
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/codex",
        Some(serde_json::json!({ "reasoning_effort": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["provider"]["reasoning_effort"], "");
    assert_eq!(json["provider"]["has_override"], false);
}

#[tokio::test]
async fn test_update_provider_null_clears_field_keeps_others() {
    let app = create_test_app();
    // まずキーと無効化を両方設定
    let (status, _) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({"api_key": "sk-keep-me", "enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 三値: enabled:null は「無効化を解除」= TOML に戻す。api_key は維持されること。
    // （旧実装では serde が null を None に潰し、この解除が無反応だった）
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({ "enabled": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let p = &json["provider"];
    assert_eq!(
        p["enabled_override"],
        serde_json::Value::Null,
        "enabled must be cleared"
    );
    // キー設定は維持 → 稼働状態のまま
    assert_eq!(p["api_key_source"], "db");
    assert_eq!(p["active"], true);
    assert_eq!(p["has_override"], true);

    // base_url:null は base_url オーバーライドだけを消す（キーは残る）
    let (_, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({ "base_url": "https://x.example" })),
    )
    .await;
    assert_eq!(json["provider"]["base_url"], "https://x.example");
    let (_, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({ "base_url": null })),
    )
    .await;
    assert_eq!(json["provider"]["base_url"], "");
    assert_eq!(
        json["provider"]["api_key_source"], "db",
        "key must survive base_url clear"
    );
}

#[tokio::test]
async fn test_voice_config_invalid_provider_not_persisted() {
    let app = create_test_app();
    // enabled + 未知の STT プロバイダ → 400、かつ DB に保存されないこと
    let bad = serde_json::json!({
        "enabled": true,
        "stt": { "provider": "nonexistent", "model": "x", "api_key_env": "X" },
        "tts": { "provider": "voicevox", "default_voice": "3" }
    });
    let (status, _) = send_request(app.clone(), "PUT", "/api/voice/config", Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // GET は依然 TOML 由来（壊れた値が db として残っていない）
    let (_, json) = send_request(app, "GET", "/api/voice/config", None).await;
    assert_eq!(json["source"], "toml");
    assert_eq!(json["config"]["enabled"], false);
}

#[tokio::test]
async fn test_voice_config_roundtrip() {
    let app = create_test_app();
    // 初期状態: TOML 由来（テストでは Default = disabled）
    let (status, json) = send_request(app.clone(), "GET", "/api/voice/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], "toml");
    assert_eq!(json["config"]["enabled"], false);
    assert_eq!(json["runtime_active"], false);

    // 保存（ランタイム停止中なので restart_required）
    let mut config = json["config"].clone();
    config["enabled"] = serde_json::json!(true);
    config["tts"]["default_voice"] = serde_json::json!("1");
    let (status, json) = send_request(app.clone(), "PUT", "/api/voice/config", Some(config)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["saved"], true);
    assert_eq!(json["applied_live"], false);
    assert_eq!(json["restart_required"], true);

    // 読み直すと DB 由来になっている
    let (_, json) = send_request(app.clone(), "GET", "/api/voice/config", None).await;
    assert_eq!(json["source"], "db");
    assert_eq!(json["config"]["tts"]["default_voice"], "1");

    // リセットで TOML に戻る
    let (status, _) = send_request(app.clone(), "DELETE", "/api/voice/config", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, json) = send_request(app, "GET", "/api/voice/config", None).await;
    assert_eq!(json["source"], "toml");
}

// ==================== Onboarding / Setup ====================

#[tokio::test]
async fn test_setup_status_fresh_and_after_agent() {
    let app = create_test_app();

    // フレッシュ DB + プロバイダ無しのルーター: 全ステップ未完。
    let (status, json) = send_request(app.clone(), "GET", "/api/setup/status", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["complete"], false);
    assert_eq!(json["next_step"], "llm_provider");
    assert_eq!(json["steps"]["llm_provider"]["done"], false);
    assert_eq!(json["steps"]["agent"]["done"], false);
    assert_eq!(json["steps"]["agent"]["count"], 0);
    assert_eq!(json["steps"]["discord"]["done"], false);
    assert_eq!(json["steps"]["channel"]["done"], false);

    // エージェントを作ると agent ステップが done + count=1 になる。
    let (_agent_id, app) = create_test_agent(app).await;
    let (status, json) = send_request(app, "GET", "/api/setup/status", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["steps"]["agent"]["done"], true);
    assert_eq!(json["steps"]["agent"]["count"], 1);
    // LLM が未設定なので next_step は依然 llm_provider。
    assert_eq!(json["next_step"], "llm_provider");
}

#[tokio::test]
async fn test_setup_seed_standard_skills() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    // OPENCRAB_SKILLS_DIR を一時ディレクトリに向け、1 件のスキルファイルを置く。
    let dir = std::env::temp_dir().join(format!("opencrab-seed-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("demo.skill.md"),
        "---\nname: demo\ndescription: \"デモスキル\"\nversion: 1\npermission: agent\nactions:\n  - send_speech\n---\n\n# デモ\n\nガイダンス。\n",
    )
    .unwrap();
    // テスト内でのみ使う（この 2 テストは env を共有しないよう別ディレクトリ）。
    std::env::set_var("OPENCRAB_SKILLS_DIR", &dir);

    let (status, json) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills/seed-standard"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["seeded_count"], 1);
    assert_eq!(json["seeded"][0], "demo");

    // 2 回目は冪等（同名スキップ）。
    let (status, json) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills/seed-standard"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["seeded_count"], 0);
    assert_eq!(json["skipped"][0], "demo");

    // シードしたスキルが一覧に出る。
    let (_, json) = send_request(app, "GET", &format!("/api/agents/{agent_id}/skills"), None).await;
    let names: Vec<String> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(names.contains(&"demo".to_string()), "skills: {names:?}");

    std::env::remove_var("OPENCRAB_SKILLS_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== Skill consolidation (sleep curation) ====================

fn state_with_consolidation(
    db: opencrab_db::Db,
    mock: Arc<MockLlmProvider>,
    cfg: opencrab_server::config::SkillConsolidationConfig,
) -> AppState {
    let mut router = LlmRouter::new();
    router.add_provider(mock as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");
    AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: cfg,
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
        #[cfg(feature = "web")]
        web_gateway: std::sync::Arc::new(opencrab_web_gateway::WebGateway::new()),
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

#[tokio::test]
async fn test_skill_consolidation_disabled_is_noop() {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let mock = Arc::new(MockLlmProvider::new());
    // enabled=false（既定）→ 何もせず false
    let state = state_with_consolidation(
        db,
        mock,
        opencrab_server::config::SkillConsolidationConfig::default(),
    );
    let ran = opencrab_server::skill_consolidation::maybe_run_skill_consolidation(&state, "a1")
        .await
        .unwrap();
    assert!(!ran);
}

#[tokio::test]
async fn test_skill_consolidation_curates_and_audits() {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let mock = Arc::new(MockLlmProvider::new());
    // 本人が「Old を retire、New を create」する判断を返す
    mock.push_text_response(
        r#"[{"name":"Old","action":"retire","reason":"もう使わない"},
            {"name":"New","action":"create","reason":"最近こう動きたい","description":"新スキル","guidance":"こうする"}]"#,
    );

    let cfg = opencrab_server::config::SkillConsolidationConfig {
        enabled: true,
        trigger_new_sessions: 1,
        time_cap_hours: 1,
        min_interval_secs: 0,
        include_archived_in_review: 3,
    };
    let state = state_with_consolidation(db.clone(), mock, cfg);

    {
        let conn = db.lock().unwrap();
        // エージェント + 既存スキル Old
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "a1".into(),
                name: "A".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "Persona".into(),
                personality: Some("好奇心旺盛".into()),
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
        opencrab_db::queries::insert_skill(
            &conn,
            &opencrab_db::queries::SkillRow {
                id: "sk-old".into(),
                agent_id: "a1".into(),
                name: "Old".into(),
                description: "d".into(),
                situation_pattern: String::new(),
                guidance: "g".into(),
                source_type: "self_created".into(),
                source_context: None,
                file_path: None,
                effectiveness: None,
                usage_count: 0,
                is_active: true,
                permission: "\"agent\"".into(),
                archived: false,
                created_caller: None,
                agent_visible: false,
            },
        )
        .unwrap();
        // 過去に棚卸し済み（cold-start シードを回避してすぐ発火させる）
        opencrab_db::queries::set_last_skill_consolidation_at(
            &conn,
            "a1",
            "2020-01-01T00:00:00+00:00",
        )
        .unwrap();
        // 新規活動（トリガの母数）
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "a1".into(),
                session_id: "sess-1".into(),
                log_type: "speech".into(),
                content: "hi".into(),
                speaker_id: Some("a1".into()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    let ran = opencrab_server::skill_consolidation::maybe_run_skill_consolidation(&state, "a1")
        .await
        .unwrap();
    assert!(ran, "consolidation should have fired");

    let conn = db.lock().unwrap();
    // Old は archived（active から消える）、New は作成されて active
    let active = opencrab_db::queries::list_skills(&conn, "a1", true).unwrap();
    let names: Vec<_> = active.iter().map(|s| s.name.as_str()).collect();
    assert!(!names.contains(&"Old"), "Old should be retired: {names:?}");
    assert!(names.contains(&"New"), "New should be created: {names:?}");
    // 監査ログ層1（agent_logs, context=sleep）
    let logs = opencrab_db::queries::list_agent_logs(&conn, Some("a1"), None, 10).unwrap();
    assert!(
        logs.iter().any(|l| l.context == "sleep"),
        "sleep audit log missing"
    );
    // last_at が前進している
    let last = opencrab_db::queries::get_last_skill_consolidation_at(&conn, "a1").unwrap();
    assert!(last.is_some() && last.as_deref() != Some("2020-01-01T00:00:00+00:00"));
}

// ==================== #169: REST の非ブロック dispatch ====================

/// REST セッションのログを新しい順に取る小さなヘルパ。
fn session_logs(
    db: &opencrab_db::Db,
    session_id: &str,
) -> Vec<opencrab_db::queries::SessionLogRow> {
    let conn = db.lock().unwrap();
    opencrab_db::queries::list_recent_session_logs(&conn, session_id, 100).unwrap()
}

fn session_status(db: &opencrab_db::Db, session_id: &str) -> Option<String> {
    let conn = db.lock().unwrap();
    opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
        .map(|s| s.status)
}

/// 即完了しない fake subtask を registry へ登録する（走行中 subtask の代役）。
fn insert_running_subtask(
    registry: &opencrab_actions::SubtaskRegistry,
    subtask_id: &str,
    parent_session_id: &str,
    agent_id: &str,
) -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(std::future::pending::<()>());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: handle.abort_handle(),
            session_id: format!("subtask-{subtask_id}"),
            parent_session_id: parent_session_id.to_string(),
            agent_id: agent_id.to_string(),
            label: "long job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Agent,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    handle
}

/// #169: `POST /api/agents/{id}/messages` はツールを background subtask として dispatch する
/// （メインを塞がない）。ツール結果は inline の `{"success":...}` ではなく
/// `{"status":"spawned"}` になり、完了本文は親セッションログへ着地する（取得口 = セッションログ）。
#[tokio::test]
async fn test_rest_message_dispatches_tool_as_background_subtask() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Dispatcher", "TestPersona").await;

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-dispatch-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "background_work",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("バックグラウンドで実行を開始しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "スキルを覚えて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = resp["session_id"].as_str().unwrap().to_string();
    assert_eq!(session_id, format!("agent-msg-{agent_id}-u1"));

    // dispatch された（tool_result が spawned）。inline 実行なら status フィールドは無い。
    let logs = session_logs(&db, &session_id);
    assert!(
        logs.iter()
            .any(|l| l.log_type == "tool_result" && l.content.contains("\"status\":\"spawned\"")),
        "REST でツールが background subtask へ dispatch されていない: {:?}",
        logs.iter()
            .map(|l| (l.log_type.clone(), l.content.clone()))
            .collect::<Vec<_>>()
    );

    // 完了本文（subtask_completed）が親セッションログへ着地する = REST の取得口。
    let mut settled = false;
    for _ in 0..100 {
        if session_logs(&db, &session_id)
            .iter()
            .any(|l| l.content.contains("subtask_completed"))
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        settled,
        "dispatch した subtask の完了本文が親セッションログへ永続化されない"
    );

    // 決着後は registry が空（settle_completed が除去する）。
    assert!(!state.subtask_registries.has_running(&session_id));
}

/// **#298**: 非ブロック dispatch した subtask は、**その run の呼び出し元**を決着通知まで
/// 運ぶ。resume する sink（Discord / web）はこの値で `RunRequest` を組むので、ここで落ちると
/// オーナー発のターンが決着の瞬間に最小権限へ降格し、`policy_allows` が owner/trusted の
/// ツールを list_tools からも dispatch からも落とす。
///
/// このテストが必要な理由: 配線点は `process.rs` の 1 箇所
/// （`SubtaskToolDispatcher::with_caller`）にしかなく、そこを外しても `crates/actions` 側の
/// ユニットテストは**自前で dispatcher を組む**ので落ちない（配線の写しでしかない）。
#[tokio::test]
async fn test_dispatched_subtask_carries_the_run_caller_to_settlement() {
    /// 決着通知を溜めるだけの sink。
    #[derive(Default)]
    struct CaptureSink(Mutex<Vec<opencrab_actions::SubtaskSettled>>);
    impl opencrab_actions::SubtaskCompletionSink for CaptureSink {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, ev: opencrab_actions::SubtaskSettled) {
            self.0.lock().unwrap().push(ev);
        }
    }

    // 昇格経路は作らない（元が `Agent` なら `Agent` のまま）ので両方を見る。
    for caller in [
        opencrab_actions::CallerIdentity::Owner,
        opencrab_actions::CallerIdentity::Agent,
    ] {
        let (app, _db, mock, state) = create_test_app_with_state();
        let (agent_id, _app) = create_test_agent_named(app, "DispatchCaller", "TestPersona").await;
        let session_id = format!("agent-msg-{agent_id}-u298");

        mock.push_tool_call_response(vec![ToolCall {
            id: "tc-298".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "learn_from_experience".to_string(),
                arguments: serde_json::json!({
                    "skill_name": "background_work",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "g"
                })
                .to_string(),
            },
        }]);
        mock.push_text_response("バックグラウンドで実行を開始しました");

        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = capture.clone();
        let run_req = opencrab_actions::RunRequest::new(
            &agent_id,
            "DispatchCaller",
            &session_id,
            "system",
            "user: スキルを覚えて",
            "rest",
            caller.clone(),
        )
        .with_dispatch(
            Some(state.subtask_registries.registry_for(&session_id)),
            sink,
        );
        opencrab_server::process::run_agent_response(&state, run_req)
            .await
            .expect("dispatch する run が失敗した");

        // 非ブロック dispatch なので決着は別タスク。CI 負荷時に取りこぼさないよう
        // 上限は 5 秒（成功時は最初の観測で即抜けるので通常はほぼ待たない）。
        let mut observed = None;
        for _ in 0..250 {
            observed = capture
                .0
                .lock()
                .unwrap()
                .first()
                .map(|ev| ev.caller.clone());
            if observed.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            observed.expect("dispatch した subtask が決着していない（前提が崩れている）"),
            caller,
            "dispatch した subtask が run の呼び出し元を運んでいない（resume が降格する）"
        );
    }
}

/// #431: `RunRequest::subtask_starts` が **両方の起動経路**から加算される。
///
/// Discord の「発言終わり」🏁 はこの数が `0` かどうかだけで「次の行動を選ばずに終わった
/// ターンか」を判定する。数え漏らすと、掘削を始めたターンに 🏁 が付き『調べますね🏁』の
/// 数分後に完了 resume の続きが届く**逆の情報**になる。
///
/// このテストが必要な理由: 配線点は `process.rs` の 2 箇所
/// （`SubtaskToolDispatcher::with_subtask_starts` と
/// `SystemGatewayActions::with_subtask_starts`）にしかなく、そこを外しても
/// `crates/actions` / `crates/server` 側のユニットテストは**自前で dispatcher や
/// gateway を組む**ので落ちない（配線の写しでしかない）。上の
/// `test_dispatched_subtask_carries_the_run_caller_to_settlement` と同じ理由。
#[tokio::test]
async fn test_run_counts_subtask_starts_from_both_launch_paths() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 決着通知を捨てるだけの sink（ここで見たいのは起動の計上だけ）。
    struct NoopSink;
    impl opencrab_actions::SubtaskCompletionSink for NoopSink {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, _ev: opencrab_actions::SubtaskSettled) {}
    }

    // (ツール名, 引数) — 左が auto-dispatch 経路、右が明示 spawn_subtask 経路。
    let cases = [
        (
            "learn_from_experience",
            serde_json::json!({
                "skill_name": "background_work",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            }),
        ),
        (
            "spawn_subtask",
            serde_json::json!({ "task": "ログを調べる", "label": "dig" }),
        ),
    ];

    for (tool_name, args) in cases {
        let (app, _db, mock, state) = create_test_app_with_state();
        let (agent_id, _app) = create_test_agent_named(app, "StartCounter", "TestPersona").await;
        let session_id = format!("agent-msg-{agent_id}-u431");

        mock.push_tool_call_response(vec![ToolCall {
            id: "tc-431".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tool_name.to_string(),
                arguments: args.to_string(),
            },
        }]);
        // 親ターンの締め。明示 spawn 経路は sub-engine も同じモックから引くので多めに積む。
        mock.push_text_response("調べますね");
        mock.push_text_response("調べますね");

        let starts = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = Arc::new(NoopSink);
        let run_req = opencrab_actions::RunRequest::new(
            &agent_id,
            "StartCounter",
            &session_id,
            "system",
            "user: ログを調べて",
            "rest",
            opencrab_actions::CallerIdentity::Owner,
        )
        .with_dispatch(
            Some(state.subtask_registries.registry_for(&session_id)),
            sink,
        )
        .with_subtask_starts(starts.clone());

        opencrab_server::process::run_agent_response(&state, run_req)
            .await
            .expect("subtask を起こす run が失敗した");

        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "{tool_name} 経路で起動した subtask が親ターンのカウンタに載っていない\
             （このターンに 🏁 が付き、数分後に続きが届く逆情報になる）"
        );
    }
}

/// #154 / #152: `POST /api/agents/{id}/web/send` もツールを background subtask として
/// dispatch する（メインを塞がない）。
///
/// このテストが必要な理由: 非ブロック dispatch の注入は `process.rs` の 1 箇所
/// （`depth == 0 && state.subtask_auto_dispatch` の分岐）で決まる。そこを潰しても
/// 従来は REST のテスト 1 本しか落ちず、web / Discord / Nostr / heartbeat は全緑
/// だった。web の配線をここで固定して、看板機能が無音で失われるのを防ぐ。
#[cfg(feature = "web")]
#[tokio::test]
async fn test_web_send_dispatches_tool_as_background_subtask() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "WebDispatcher", "TestPersona").await;

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-web-dispatch-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "web_background_work",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("バックグラウンドで実行を開始しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/web/send"),
        Some(serde_json::json!({
            "conversation_id": "conv-dispatch",
            "content": "スキルを覚えて",
            "user_id": "u1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = resp["session_id"].as_str().unwrap().to_string();
    assert_eq!(session_id, format!("web-{agent_id}-conv-dispatch"));

    // dispatch された（tool_result が spawned）。inline 実行なら status フィールドは無い。
    let logs = session_logs(&db, &session_id);
    assert!(
        logs.iter()
            .any(|l| l.log_type == "tool_result" && l.content.contains("\"status\":\"spawned\"")),
        "web でツールが background subtask へ dispatch されていない: {:?}",
        logs.iter()
            .map(|l| (l.log_type.clone(), l.content.clone()))
            .collect::<Vec<_>>()
    );

    // 完了本文（subtask_completed）が親セッションログへ着地する。
    let mut settled = false;
    for _ in 0..100 {
        if session_logs(&db, &session_id)
            .iter()
            .any(|l| l.content.contains("subtask_completed"))
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        settled,
        "web で dispatch した subtask の完了本文が親セッションログへ永続化されない"
    );

    // 決着後は web gateway 側の registry も空になる（`cancel_subtask` の到達先と同一）。
    assert!(
        !state.web_gateway.has_running(&session_id),
        "決着後も web gateway の registry にエントリが残っている"
    );
}

/// #632: 存在しない `agent_id` への `web/send` は**ターンを起こさず 404**。
///
/// `agents` 行が無いと per-agent 設定が全部既定に落ちるのに「動いてしまう」ため、
/// タイプミスに気づけない。合成済み Router 越しに **LLM ターンが 1 度も回らない**ことを
/// 固定する（存在確認は `run_and_deliver_serialized` チョークポイントが担う）。
// #654: `/web/send` ルートは web feature 時のみマウントされる（#651）。off ではルート不在で
// 404/契約が変わり検証が成立しないので同じ cfg で囲む。
#[cfg(feature = "web")]
#[tokio::test]
async fn test_web_send_unknown_agent_is_rejected_without_running() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let bogus = "does-not-exist-632";

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{bogus}/web/send"),
        Some(serde_json::json!({
            "conversation_id": "conv-x",
            "content": "存在しないエージェントに投げる",
            "user_id": "u1"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(resp["error"], format!("agent not found: {bogus}"));
    // ターンが走らない = LLM が 1 度も呼ばれない。
    assert!(
        mock.system_prompts().is_empty(),
        "存在しないエージェントで LLM ターンが走った"
    );
}

/// #632 回帰: 存在するエージェントは `web/send` で従来どおり 200 でターンが走る。
// #654: `/web/send` ルートは web feature 時のみマウントされる（#651）。off はルート不在なので同じ cfg で囲む。
#[cfg(feature = "web")]
#[tokio::test]
async fn test_web_send_existing_agent_runs() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "WebReal", "TestPersona").await;
    mock.push_text_response("やあ");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/web/send"),
        Some(serde_json::json!({
            "conversation_id": "conv-ok",
            "content": "hi",
            "user_id": "u1"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "存在するエージェントは 200 で走る");
    assert_eq!(resp["session_id"], format!("web-{agent_id}-conv-ok"));
    assert!(resp["response"].is_string(), "応答本文が返る: {resp}");
}

/// #632: 存在しない `agent_id` への REST `POST /api/agents/{id}/messages` も同じ穴を
/// 持っていた。サーバ側チョークポイント（`process::run_agent_response`）で弾き、404 に揃える。
#[tokio::test]
async fn test_rest_messages_unknown_agent_is_rejected_without_running() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let bogus = "does-not-exist-632-rest";

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{bogus}/messages"),
        Some(serde_json::json!({ "content": "hi", "user_id": "u1" })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(resp["error"], format!("agent not found: {bogus}"));
    assert!(
        mock.system_prompts().is_empty(),
        "存在しないエージェントで LLM ターンが走った"
    );
}

/// #632 回帰: 存在するエージェントは REST `POST /messages` で従来どおり 200 で走る。
#[tokio::test]
async fn test_rest_messages_existing_agent_runs() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "RestReal", "TestPersona").await;
    mock.push_text_response("やあ");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({ "content": "hi", "user_id": "u1" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "存在するエージェントは 200 で走る");
    assert_eq!(resp["session_id"], format!("agent-msg-{agent_id}-u1"));
    assert!(resp["responses"].is_array(), "応答配列が返る: {resp}");
}

/// #632（第 3 の入口）: `POST /api/sessions/{id}/messages` は存在しない participant で
/// ターンを起こしていた（`create_session` が participant の存在を確認せず、`agent_sessions`
/// に FK が無く、参加者ループが `run_agent_response` を呼ぶため）。サーバ側チョークポイントで
/// 弾き、404 に揃える。**でたらめな participant の LLM ターンが走らない**ことを固定する。
#[tokio::test]
async fn test_session_messages_unknown_participant_is_rejected_without_running() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let bogus = "does-not-exist-632-participant";

    // でたらめな participant でセッションを作る（create_session は存在確認しない）。
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "t",
            "participant_ids": [bogus]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // send（sender は participant ループの対象外なので bogus participant が走る対象になる）。
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": "sender-not-a-participant",
            "content": "hi"
        })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(resp["error"], format!("agent not found: {bogus}"));
    assert!(
        mock.system_prompts().is_empty(),
        "存在しない participant で LLM ターンが走った"
    );
}

// ==================== #640: REST ターンの直列化 ====================

/// `chat_completion` の中に同時に何本の run が居たかを観測する LLM プロバイダ。
///
/// `hold` の間 sleep して重なりの窓を作る。REST の `run_agent_response` が共有ロック
/// （`state.session_locks.run_serialized`）を通っていれば、同一セッションの run は重ならず
/// `max_in_flight` は 1 を超えない。別セッションは重なり 2 以上になりうる。web の
/// `same_session_serializes` / `different_sessions_run_concurrently` と同型の観測。
struct SerializationProbe {
    in_flight: std::sync::atomic::AtomicUsize,
    max_in_flight: std::sync::atomic::AtomicUsize,
    hold: std::time::Duration,
}

impl SerializationProbe {
    fn new(hold: std::time::Duration) -> Self {
        Self {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            max_in_flight: std::sync::atomic::AtomicUsize::new(0),
            hold,
        }
    }

    fn max(&self) -> usize {
        self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for SerializationProbe {
    fn name(&self) -> &str {
        "mock"
    }

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.hold).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("ok"),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage::default(),
            created: 0,
        })
    }
}

/// `SerializationProbe` を既定プロバイダに仕込んだ app を作る。既定モデル `mock:test` が
/// probe（`name()=="mock"`）へ解決する。
fn create_probe_app(
    hold: std::time::Duration,
) -> (Router, opencrab_db::Db, Arc<SerializationProbe>) {
    let (mut state, db) = create_test_state(0.5);
    let probe = Arc::new(SerializationProbe::new(hold));
    let mut router = LlmRouter::new();
    router.add_provider(probe.clone() as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");
    state.llm_router = opencrab_server::SharedLlmRouter::new(router);
    // #703: 既定モデル `mock:test` を `model_pricing` に登録する。#676 で「出力上限が未登録の
    // モデルは fail loud」になったため、登録が無いと run が LLM へ届く前に止まり、probe が
    // 1 度も呼ばれない（HTTP は 200 のままで、本文にエラー文字列が入るだけなので気づけない）。
    // このヘルパを使うテストは**直列化**を見るものなので、モデル解決で落ちてはいけない。
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "mock".to_string(),
                model: "test".to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(4_096),
            },
        )
        .expect("probe 用モデルの登録");
    }
    (opencrab_server::create_router(state), db, probe)
}

/// #640: 同一セッション（同じ agent_id + user_id）への並行 2 POST は直列に走る
/// （`run_agent_response` の中で同時に走っている run が 1 を超えない）。
#[tokio::test]
async fn test_rest_agent_messages_serialize_same_session() {
    let (app, _db, probe) = create_probe_app(std::time::Duration::from_millis(150));
    let (agent_id, app) = create_test_agent_named(app, "SerialRest", "TestPersona").await;

    let a1 = app.clone();
    let id1 = agent_id.clone();
    let h1 = tokio::spawn(async move {
        send_request(
            a1,
            "POST",
            &format!("/api/agents/{id1}/messages"),
            Some(serde_json::json!({"content": "m1", "user_id": "u1"})),
        )
        .await
    });
    let a2 = app.clone();
    let id2 = agent_id.clone();
    let h2 = tokio::spawn(async move {
        send_request(
            a2,
            "POST",
            &format!("/api/agents/{id2}/messages"),
            Some(serde_json::json!({"content": "m2", "user_id": "u1"})),
        )
        .await
    });

    let (s1, _) = h1.await.unwrap();
    let (s2, _) = h2.await.unwrap();
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        probe.max(),
        1,
        "同一セッションへの並行 POST の run が重なった（直列化が効いていない）"
    );
}

/// #640: 別セッション（別 user_id）への並行 POST は従来どおり並行に走る
/// （ロックの粒度は session_id 単位。無関係なセッションを詰まらせない）。
#[tokio::test]
async fn test_rest_agent_messages_run_concurrently_across_sessions() {
    let (app, _db, probe) = create_probe_app(std::time::Duration::from_millis(300));
    let (agent_id, app) = create_test_agent_named(app, "ConcurrentRest", "TestPersona").await;

    let a1 = app.clone();
    let id1 = agent_id.clone();
    let h1 = tokio::spawn(async move {
        send_request(
            a1,
            "POST",
            &format!("/api/agents/{id1}/messages"),
            Some(serde_json::json!({"content": "m1", "user_id": "userA"})),
        )
        .await
    });
    let a2 = app.clone();
    let id2 = agent_id.clone();
    let h2 = tokio::spawn(async move {
        send_request(
            a2,
            "POST",
            &format!("/api/agents/{id2}/messages"),
            Some(serde_json::json!({"content": "m2", "user_id": "userB"})),
        )
        .await
    });

    let (s1, _) = h1.await.unwrap();
    let (s2, _) = h2.await.unwrap();
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        probe.max(),
        2,
        "別セッションへの並行 POST が直列化された（粒度が session ではなく global になっている）"
    );
}

/// GET を 1 本流し、**body を読まずに** `(status, content-type)` を返す。
///
/// `send_request` は body を読み切るので、終端しない SSE レスポンス（`web/stream`）には
/// 使えない。ハングを「失敗」として観測できるよう、ヘッダ取得にタイムアウトを掛ける。
#[cfg(feature = "web")]
async fn get_head(app: Router, uri: &str) -> (StatusCode, Option<String>) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let res = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
        .await
        .expect("router がレスポンスヘッダを返さない（ハングしている）")
        .expect("router がリクエストを落とした");
    let content_type = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // body（SSE ストリーム）は読まずに drop する。
    (res.status(), content_type)
}

/// #190 S4: **購読側のルートが server の合成済み Router に取り付いていること**。
///
/// web gateway のルート定義は独立クレートへ移設され（`opencrab_web_gateway::routes()`）、
/// server は `create_router` の `.merge(...)` で取り付けるだけになった。クレート側の
/// テストはルータ単体（`routes()`）を叩くので、**server の合成が壊れても気づけない**。
/// 送信側（`web/send`）は上の dispatch/resume テストが合成済み Router 越しに叩いている
/// が、購読側（`web/stream`）には in-process の検証が無かった。
///
/// これが落ちる変異: `create_router` の `.merge(opencrab_web_gateway::routes::<AppState>())`
/// を消す / 購読側のパス文字列を変える / クエリ名 `conversation` を変える。
///
/// SSE の body は終端しないので読まない（**接続が確立してルートが解決されること**が主眼）。
#[cfg(feature = "web")]
#[tokio::test]
async fn test_web_stream_route_is_mounted_on_the_server_router() {
    let app = create_test_app();
    // 購読は DB を触らない（agent 行の有無に依存しない）。パスの解決だけを見る。
    let agent_id = "mounted-agent";

    // 1. 購読ルートが載っている: 404 ではなく 200 + SSE の content-type が返る。
    let (status, content_type) = get_head(
        app.clone(),
        &format!("/api/agents/{agent_id}/web/stream?conversation=conv-mounted"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "server の合成済み Router に GET /api/agents/{{id}}/web/stream が載っていない"
    );
    assert!(
        content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/event-stream")),
        "SSE ハンドラに解決されていない（content-type: {content_type:?}）"
    );

    // 2. クエリ名 `conversation` も契約（ダッシュボードが組み立てる URL）。
    //    名前が変わると extractor が 400 を返すので、404 とは区別できる。
    let (status, _) = get_head(app.clone(), &format!("/api/agents/{agent_id}/web/stream")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "`conversation` が必須クエリでなくなっている"
    );

    // 3. 対照: 取り付けていないパスは 404 になる（1. の「404 でない」に意味を持たせる）。
    for uri in [
        format!("/api/agents/{agent_id}/web/streams?conversation=c"),
        format!("/api/agents/{agent_id}/web/stream/extra?conversation=c"),
        format!("/api/agents/{agent_id}/web"),
    ] {
        let (status, _) = get_head(app.clone(), &uri).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri} は取り付けたルートの担当外のはず"
        );
    }
}

/// SSE チャンネルから指定 `kind` のイベントが届くまで待つ（テスト用ヘルパ）。
///
/// resume は sink → `tokio::spawn` の非同期経路なので、待たずに読むと取りこぼす。
/// 途中で流れる別 `kind`（inbound の `direct` など）は読み飛ばす。
#[cfg(feature = "web")]
async fn recv_web_event_of_kind(
    rx: &mut tokio::sync::broadcast::Receiver<String>,
    kind: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let needle = format!("\"kind\":\"{kind}\"");
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(payload)) if payload.contains(&needle) => return Some(payload),
            // 別 kind のイベント（inbound の direct 等）は読み飛ばす。
            Ok(Ok(_)) => continue,
            // チャンネル終了 / lag / タイムアウトは「届かなかった」扱い。
            Ok(Err(_)) | Err(_) => return None,
        }
    }
}

/// #152 / #154 [P2]: **バックグラウンド実行の結果が web の会話へ再注入される**
/// （看板機能の後半）ことを in-process で固定する。
///
/// このテストが必要な理由: 再注入は `WebCompletionSink::on_subtask_settled` の 1 箇所で
/// 起きるが、そこを丸ごと `return;` にしても opencrab-server のテストは全て緑だった。
/// 唯一の検証が `#[ignore]` + 実 LLM + 稼働サーバ前提の E2E で、CI では走らなかった。
///
/// 3 つの独立した証拠で resume の実行を確認する:
/// 1. SSE へ `kind:"subtask_resume"` のイベントが流れる（配送）。
/// 2. resume ターンの LLM リクエストの system prompt に `[subtask_completed: subtask_id=`
///    が注入されている（会話への再注入）。
/// 3. resume ターンの発話が親セッションの履歴へ残る（永続化）。
#[cfg(feature = "web")]
#[tokio::test]
async fn test_web_subtask_completion_resumes_parent_conversation() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "WebResumer", "TestPersona").await;
    let session_id = format!("web-{agent_id}-conv-resume");

    // resume は非同期に発火するので、送信前に購読しておく（取りこぼし防止）。
    let mut rx = state.web_gateway.subscribe(&session_id);

    // 1 ターン目: ツール呼び出し → dispatch → spawned を見た 2 回目で本文を返す。
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-web-resume-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "web_resume_work",
                "experience": "e",
                "outcome": "success",
                "lesson": "l",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("バックグラウンドで実行を開始しました");
    // 3 回目 = subtask 完了後の resume ターン。
    mock.push_text_response("スキルの学習が完了しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/web/send"),
        Some(serde_json::json!({
            "conversation_id": "conv-resume",
            "content": "スキルを覚えて",
            "user_id": "u1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["session_id"].as_str().unwrap(), session_id);

    // 証拠 1: resume の応答が SSE へ配送される。
    let payload =
        recv_web_event_of_kind(&mut rx, "subtask_resume", std::time::Duration::from_secs(5))
            .await
            .unwrap_or_else(|| {
                // system prompt 全文は巨大なので、件数と完了マーカーの有無だけを出す。
                let prompts = mock.system_prompts();
                let with_marker = prompts
                    .iter()
                    .filter(|p| p.contains("[subtask_completed: subtask_id="))
                    .count();
                panic!(
                    "subtask 完了後に resume の SSE イベントが流れない（再注入が動いていない）。\
             LLM 呼び出し回数={} うち完了マーカー入り={with_marker}",
                    prompts.len()
                )
            });
    assert!(
        payload.contains("スキルの学習が完了しました"),
        "resume イベントの本文が resume ターンの応答ではない: {payload}"
    );

    // 証拠 2: resume ターンの system prompt に完了マーカーが注入されている。
    let prompts = mock.system_prompts();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("[subtask_completed: subtask_id=")),
        "resume ターンの system prompt に完了マーカーが注入されていない: {prompts:?}"
    );

    // 証拠 3: resume ターンの発話が親セッションの履歴へ残る（再読込しても消えない）。
    let mut persisted = false;
    for _ in 0..100 {
        if session_logs(&db, &session_id)
            .iter()
            .any(|l| l.content.contains("スキルの学習が完了しました"))
        {
            persisted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        persisted,
        "resume ターンの発話が親セッションの履歴へ保存されない"
    );
}

/// #152 [P2 回帰]: **進捗通知（`SettleKind::Progress`）では resume しない**。
///
/// 走行中の subtask の途中経過で親を resume すると、まだ走っている run の最中に
/// 二重で応答してしまう。`WebCompletionSink` の `kind != Completed` ガードを外すと
/// このテストが落ちる。
///
/// 対比として同じ sink に `Completed` を投げ、そちらでは resume が走ることも確認する
/// （sink 全体を no-op にしただけでも落ちるようにするため）。
#[cfg(feature = "web")]
#[tokio::test]
async fn test_web_progress_settlement_does_not_resume() {
    use opencrab_actions::{SettleKind, SubtaskSettled};
    use opencrab_web_gateway::WebCompletionSink;

    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "WebProgress", "TestPersona").await;
    let session_id = format!("web-{agent_id}-conv-progress");
    let mut rx = state.web_gateway.subscribe(&session_id);

    // SSE チャンネルと直列化ロックは runner（AppState）から引かれる。
    let sink = WebCompletionSink::new(state.clone());
    let settled = |kind: SettleKind| SubtaskSettled {
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        subtask_id: "st-progress-1".to_string(),
        exit_reason: "progress".to_string(),
        kind,
        reply_target: None,
        caller: opencrab_actions::CallerIdentity::Agent,
    };

    // 進捗通知: resume しない = LLM を一度も呼ばない / SSE へ何も流れない。
    // （応答生成が走ると、LLM のキューが空でも `kind:"error"` が SSE へ流れるので、
    //   「特定 kind が来ない」ではなく「何も来ない」を要求する。）
    opencrab_actions::dispatch_settled(&sink, settled(SettleKind::Progress));
    let stray = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;
    assert!(
        stray.is_err(),
        "進捗通知（Progress）で resume してしまっている（走行中の run に二重応答する）: {stray:?}"
    );
    assert_eq!(
        mock.system_prompts().len(),
        0,
        "進捗通知で LLM 応答生成が走っている（resume してはならない）"
    );

    // 対比: 完了通知なら resume が走る（このガードが「効きすぎ」でないことの確認）。
    mock.push_text_response("完了しました");
    opencrab_actions::dispatch_settled(&sink, settled(SettleKind::Completed));
    assert!(
        recv_web_event_of_kind(&mut rx, "subtask_resume", std::time::Duration::from_secs(5))
            .await
            .is_some(),
        "完了通知（Completed）で resume が走らない"
    );
}

/// #169: registry が `AppState` 経由で共有されるため、REST から `cancel_subtask` が
/// 走行中 subtask に到達できる（使い捨て registry では常に not found だった）。
#[tokio::test]
async fn test_rest_cancel_subtask_reaches_shared_registry() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Canceller", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    // ハンドラが使うのと同一の registry（AppState 保持）へ走行中 subtask を登録する。
    let registry = state.subtask_registries.registry_for(&session_id);
    let handle = insert_running_subtask(&registry, "st-rest-1", &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-cancel-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "cancel_subtask".to_string(),
            arguments: serde_json::json!({"subtask_id": "st-rest-1"}).to_string(),
        },
    }]);
    mock.push_text_response("止めました");

    let (status, _resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "さっきのを止めて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 停止が到達した: registry から除去され、タスクが abort されている。
    assert!(
        !registry.contains_key("st-rest-1"),
        "cancel_subtask が共有 registry に到達していない（not found）"
    );
    assert!(handle.await.unwrap_err().is_cancelled());

    let logs = session_logs(&db, &session_id);
    assert!(
        logs.iter().any(|l| l.log_type == "tool_cancelled"),
        "親セッションログに tool_cancelled が記録されない"
    );
    assert!(
        !logs
            .iter()
            .any(|l| l.log_type == "tool_result" && l.content.contains("not found")),
        "cancel_subtask が not found を返している: {:?}",
        logs.iter()
            .filter(|l| l.log_type == "tool_result")
            .map(|l| l.content.clone())
            .collect::<Vec<_>>()
    );
}

// ================================================================================
// #203 / #184: REST + Discord の実配線で「最後の走行中 subtask の停止」がセッションを
// 完了させることの e2e 固定（実際に起きていた不具合の再発防止）。
//
// #204 より前の壊れ方: 合成層（`SystemGatewayActions`）が「inner が `cancel_subtask` を
// 定義していれば inner へ委譲」していたため、Discord が有効だと停止が Discord 実装へ
// 流れ、REST の完了受け口（`RestCompletionSink::on_subtask_cancelled`）が**一度も
// 呼ばれず**、セッションが永久に `active` のまま残っていた。#204 で委譲を撤去したが、
// 「transport gateway が inner として配線された構成」を実際に作るテストが無かったため、
// 配線全体（共有 registry → 停止の実体 → 停止 sink → `sessions.status`）が繋がって
// いることは読解でしか裏付けられていなかった。
//
// ## なぜ HTTP エンドポイントを叩かないのか
//
// `send_agent_message` は run のあとに必ず `complete_session_if_idle`（registry が空なら
// `completed`）を通す。つまり **sink が一度も呼ばれなくても、同じリクエストの終わりで
// セッションは `completed` になる** = HTTP 層の観測ではこの不具合を検知できない
// （旧実装でも緑になる）。そこで停止ターンだけはハンドラ step 9 と同一の `RunRequest`
// （REST の sink + 共有 registry + transport gateway を inner）を組んで
// `process::run_agent_response` を直接呼び、完了が **停止 sink 経由でだけ**起きることを
// 観測する。実ネットワークには出ない（`DiscordGatewayActions::from_token` が内部で
// 組む Http クライアントは接続しない）。
// ================================================================================

/// 「走行中 subtask を 1 本抱えた REST セッション」を作り、`inner` を transport gateway
/// として配線した run から `cancel_subtask` を呼ぶ。
///
/// 返り値は [`CancelObservation`]（assert は呼び出し側が行う）。
///
/// `make_inner` はハンドラと同じ材料（共有 DB / workspace_base）から transport gateway を
/// 組むためのファクトリ。本番（`send_agent_message` step 6）と同じく `state.db` を渡す。
#[cfg(feature = "discord")]
async fn cancel_last_subtask_in_rest_run_with_inner(
    make_inner: impl FnOnce(opencrab_db::Db, String) -> Arc<dyn opencrab_gateway::GatewayActions>,
) -> CancelObservation {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "DiscordWired", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    // 1. 走行中 subtask を共有 registry へ入れた状態で HTTP を 1 回通し、sessions 行を
    //    作りつつ `active` のまま残す（= 本番の「dispatch 済みでまだ走っている」状態）。
    let registry = state.subtask_registries.registry_for(&session_id);
    let handle = insert_running_subtask(&registry, "st-dw-1", &session_id, &agent_id);
    mock.push_text_response("走らせています");
    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "長いのをやって", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        session_status(&db, &session_id).as_deref(),
        Some("active"),
        "前提が崩れている: 走行中 subtask があるのに session が active でない"
    );

    // 2. 停止ターン。`send_agent_message` step 9 と同一の RunRequest を組む。
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-cancel-dw".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "cancel_subtask".to_string(),
            arguments: serde_json::json!({"subtask_id": "st-dw-1"}).to_string(),
        },
    }]);
    mock.push_text_response("止めました");

    let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> =
        Arc::new(opencrab_server::api::agents_messages::RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
            state: state.clone(),
            agent_name: "DiscordWired".to_string(),
        });
    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        "DiscordWired",
        &session_id,
        "system",
        "user: さっきのを止めて",
        "rest",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_dispatch(Some(registry.clone()), sink)
    .with_gateway_actions(make_inner(db.clone(), state.workspace_base.clone()));
    opencrab_server::process::run_agent_response(&state, run_req)
        .await
        .expect("停止ターンの run が失敗した");

    // 観測だけして返す（assert は呼び出し側。症状 = `sessions.status` を先に主張させたい）。
    let removed_from_registry = !registry.contains_key("st-dw-1");
    let aborted = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .map(|r| r.unwrap_err().is_cancelled())
        .unwrap_or(false);
    CancelObservation {
        session_status: session_status(&db, &session_id),
        removed_from_registry,
        aborted,
    }
}

/// [`cancel_last_subtask_in_rest_run_with_inner`] の観測結果。
#[cfg(feature = "discord")]
struct CancelObservation {
    /// 停止後の親セッションの `sessions.status`（本題。`completed` でなければ #184 の再発）。
    session_status: Option<String>,
    /// 共有 registry から当該 subtask が外れたか。
    removed_from_registry: bool,
    /// subtask のタスクが実際に abort されたか。
    aborted: bool,
}

/// **#184 の実害バグの e2e 固定**: Discord の gateway actions を実際に inner として
/// 配線した REST の run で最後の走行中 subtask を停止すると、セッションが `completed`
/// になる（停止 sink が発火する唯一の経路）。
///
/// 落ちるとき: 合成層が停止を own で処理しなくなったとき（inner へ委譲する / own の
/// 分岐から sink 通知が抜けるなど）。Discord は #204 以降 `cancel_subtask` を定義しない
/// ので、委譲すれば `Unknown action` になり sink は呼ばれない。
#[cfg(feature = "discord")]
#[tokio::test]
async fn test_rest_cancel_completes_session_with_discord_gateway_wired() {
    let obs = cancel_last_subtask_in_rest_run_with_inner(|db, workspace_base| {
        // 接続しない（Http クライアントを組むだけ）。Discord API は一度も叩かない。
        // serenity の型は discord クレート内（from_token）に閉じる。
        Arc::new(opencrab_discord::DiscordGatewayActions::from_token(
            "dummy-token",
            db,
            workspace_base,
            None,
        ))
    })
    .await;
    assert_eq!(
        obs.session_status.as_deref(),
        Some("completed"),
        "REST + Discord 配線で最後の走行中 subtask を停止したのにセッションが completed に\
         ならない（RestCompletionSink::on_subtask_cancelled が呼ばれていない = #184 の再発）"
    );
    assert!(
        obs.removed_from_registry,
        "停止が共有 registry に到達していない（not found のまま）"
    );
    assert!(obs.aborted, "停止したのに subtask が abort されていない");
}

/// **#204 前の構成そのものの再現**: inner（Discord 相当）が `cancel_subtask` を**同名で
/// 定義していても**、停止は own が処理してセッションが `completed` になる。
///
/// 落ちるとき: 合成層の停止を `report_progress` と同じ「inner が定義していれば委譲」
/// パターンに戻したとき。委譲先は sink を触らないので、セッションは `active` のまま残る
/// （= #184 で報告された永久 active そのもの）。
#[cfg(feature = "discord")]
#[tokio::test]
async fn test_rest_cancel_completes_session_even_if_inner_defines_cancel_subtask() {
    /// 実際の Discord gateway actions に「`cancel_subtask` の定義と実装」を足した inner。
    /// #204 で撤去した旧 Discord 実装と同じ形（sink を触らずに成功を返す）。
    struct CancelDefiningInner {
        discord: opencrab_discord::DiscordGatewayActions,
        cancel_calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl opencrab_gateway::GatewayActions for CancelDefiningInner {
        fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
            let mut defs = self.discord.definitions();
            defs.push(opencrab_gateway::GatewayActionDef {
                name: "cancel_subtask".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "discord cancel (旧実装相当)".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"subtask_id": {"type": "string"}},
                    "required": ["subtask_id"]
                }),
            });
            defs
        }

        async fn execute(
            &self,
            name: &str,
            args: &serde_json::Value,
            ctx: &opencrab_gateway::GatewayCallContext,
        ) -> opencrab_gateway::GatewayActionResult {
            if name == "cancel_subtask" {
                self.cancel_calls
                    .lock()
                    .unwrap()
                    .push(args["subtask_id"].as_str().unwrap_or("?").to_string());
                // 旧 Discord 実装は完了 sink を知らない = セッション整合を取らない。
                return opencrab_gateway::GatewayActionResult {
                    success: true,
                    data: Some(serde_json::json!({"cancelled": true, "reached_inner": true})),
                    error: None,
                };
            }
            self.discord.execute(name, args, ctx).await
        }
    }

    let cancel_calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = cancel_calls.clone();
    let obs = cancel_last_subtask_in_rest_run_with_inner(move |db, workspace_base| {
        Arc::new(CancelDefiningInner {
            discord: opencrab_discord::DiscordGatewayActions::from_token(
                "dummy-token",
                db,
                workspace_base,
                None,
            ),
            cancel_calls: recorded,
        })
    })
    .await;

    let delegated = cancel_calls.lock().unwrap().clone();
    assert_eq!(
        obs.session_status.as_deref(),
        Some("completed"),
        "inner が cancel_subtask を定義していると停止 sink が落ちてセッションが永久 active に\
         なる（#184 の再発 / 委譲パターンへの逆戻り）。inner へ届いた停止: {delegated:?}"
    );
    assert!(
        delegated.is_empty(),
        "cancel_subtask が inner へ委譲されている（own が処理しなければ sink が発火しない）: {delegated:?}"
    );
    assert!(
        obs.removed_from_registry,
        "停止が共有 registry に到達していない（not found のまま）"
    );
    assert!(obs.aborted, "停止したのに subtask が abort されていない");
}

/// #169: 走行中 subtask があるあいだは session を `completed` にしない。
#[tokio::test]
async fn test_rest_session_stays_active_while_subtask_runs() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Runner", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    let registry = state.subtask_registries.registry_for(&session_id);
    let handle = insert_running_subtask(&registry, "st-running", &session_id, &agent_id);

    mock.push_text_response("走らせています");
    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "進捗どう", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        session_status(&db, &session_id).as_deref(),
        Some("active"),
        "走行中 subtask があるのに session が completed になっている"
    );
    handle.abort();
}

/// #169 非退行: 走行中 subtask が無ければ従来どおり応答後に `completed` になる。
#[tokio::test]
async fn test_rest_session_completed_when_no_subtask_runs() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Plain", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    mock.push_text_response("できました");
    let (status, _) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "やって", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        session_status(&db, &session_id).as_deref(),
        Some("completed")
    );
}

/// #169: 最後の subtask が決着した時点で `RestCompletionSink` が session を完了させる
/// （走行中は active のままなので、誰かが最後に完了させないと永久 active になる）。
#[tokio::test]
async fn test_rest_sink_completes_session_after_last_subtask_settles() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Sinker", "TestPersona").await;

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-sink-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "sink_check",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("開始しました");
    // #638: subtask の決着が**継続ターン**を起こすようになったので、その 1 本分の応答も要る
    // （以前は REST だけ継続しなかったため 2 本で足りていた）。継続ターンが終わってから
    // `sessions.status` の整合が行われる。本文は #631 の最小再現（`HELLO_631` を返させる）に
    // 合わせ、**継続ターンの応答だと一意に分かる文言**にする——下でセッションログに
    // この本文が残ることを assert し、「継続が走った」だけでなく「結果が読める」ことまで留める。
    mock.push_text_response("HELLO_631 を確認しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "覚えて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = resp["session_id"].as_str().unwrap().to_string();

    let mut completed = false;
    for _ in 0..100 {
        if session_status(&db, &session_id).as_deref() == Some("completed") {
            completed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        completed,
        "全 subtask 決着後も session が completed にならない"
    );
    assert!(!state.subtask_registries.has_running(&session_id));

    // #638/#631 の実症状の錠前: 継続ターンの**本文がセッションログに残る**こと。
    //
    // status が completed になるだけでは「継続が走った」までしか言えない。#631 で利用者が
    // 困っていたのは「subtask の結果を受けた続きの発話が返ってこない」ことなので、その発話が
    // `GET /api/sessions/{id}/logs` の源（memory_sessions）へ永続化されるところまで留める。
    // 継続を削るとこの assert が落ちる（`mock` の 3 本目が消費されないため文言も現れない）。
    let logs = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    assert!(
        logs.iter().any(|l| l.content.contains("HELLO_631")),
        "継続ターンの応答がセッションログに残っていない（#631: 結果を受けた続きが読めない）: {:?}",
        logs.iter().map(|l| l.content.as_str()).collect::<Vec<_>>()
    );
}

/// **nsec（Nostr 秘密鍵）は設定取得 API の応答に平文で現れない**（#203 の一括点検）。
///
/// `GET /api/agents/{id}/nostr` は `secret_key_masked` にマスク済みの値を載せる契約だが、
/// マスク関数を素通しに書き換えても落ちるテストが 1 件も無かった。nsec は Nostr の
/// アイデンティティそのもので、漏れれば第三者がそのエージェントとして投稿できる。
/// マスクの**戻り値**ではなく **API の応答ボディ全体**に平文が含まれないことを見る
/// （経路が違えば別のフィールドから漏れうるため）。
// #654: `/api/agents/{id}/nostr` ルートは nostr feature 時のみマウントされる（#651）。off では
// ルート不在で保存/取得の契約が成立しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn test_get_nostr_config_never_returns_raw_secret_key() {
    let app = create_test_app();
    // 本物と同じ形の、しかしテスト専用のダミー nsec。
    let nsec = "nsec1testonlyfakesecretkeyvalue000000000000000000000000000000";

    // 保存（enabled=false なのでゲートウェイは起動しない = ネットワークに出ない）。
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/agents/a1/nostr",
        Some(serde_json::json!({
            "secret_key": nsec,
            "relays": ["wss://relay.example"],
            "keywords": ["crab"],
            "enabled": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(
        !json.to_string().contains(nsec),
        "保存の応答に平文 nsec が含まれている: {json}"
    );

    // 取得。
    let (status, json) = send_request(app.clone(), "GET", "/api/agents/a1/nostr", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["configured"], true);
    assert_eq!(json["has_secret_key"], true, "鍵の有無は伝える: {json}");
    assert!(
        !json.to_string().contains(nsec),
        "取得の応答に平文 nsec が含まれている: {json}"
    );
    // 平文の断片も出さない（末尾数文字を見せる形のマスクへ緩めたら落とす）。
    assert!(
        !json.to_string().contains("testonlyfake"),
        "nsec の一部が応答に含まれている: {json}"
    );
    assert_eq!(
        json["secret_key_masked"], "••••••••",
        "マスク済みの固定文字列を返す: {json}"
    );
}

/// 平文（非 JSON）のエラーボディを読む。
///
/// `send_request` は JSON として解釈できないボディをバイト列の配列にして返すため、
/// エラー文言をそのまま `contains` できない。
// #654: この helper を使うのは nostr feature 依存の鍵払い出し e2e（#651）だけなので同じ cfg で囲む。
#[cfg(feature = "nostr")]
fn plain_body(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(bytes) => {
            let raw: Vec<u8> = bytes
                .iter()
                .filter_map(|b| b.as_u64().map(|n| n as u8))
                .collect();
            String::from_utf8_lossy(&raw).into_owned()
        }
        other => other.to_string(),
    }
}

/// **鍵の払い出しの受け口が無い構成では 503 で失敗する**（#191 段階2 PR4）。
///
/// 鍵生成は transport 固有の操作で、`AppState` の名指しフィールドから
/// capability の受け口（登録簿 → `key_provisioning`）へ移した。ハーネスの
/// `AppState` は登録簿が空なので受け口が引けない。ここが「無ければ黙って既定の
/// 外部コマンドを叩く」側へ倒れると、REST から想定外のバイナリで鍵を生成し
/// うる（= 外部プロセスの spawn が無言で起きる）ため、**明示的に失敗する**こと
/// と**文言が変わっていない**ことを固定する。
///
/// 判定の位置も仕様: prefix の書式検証（400）より後、鍵の生成より手前。
// #654: `/api/agents/{id}/nostr/generate` ルートは nostr feature 時のみマウントされる（#651）。
// off ではルート不在で 503 契約が成立しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn test_generate_nostr_key_fails_without_key_provisioning() {
    let app = create_test_app();

    let (status, body) = send_request(
        app.clone(),
        "POST",
        "/api/agents/a1/nostr/generate",
        Some(serde_json::json!({"prefix": "cr"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "受け口が無ければ 503（黙って既定の外部コマンドへ倒さない）: {body}"
    );
    assert!(
        plain_body(&body).contains("Nostr マネージャが無効です"),
        "エラー文言は据え置き: {body}"
    );

    // 書式が不正な prefix は受け口の有無より**手前**で 400（無効な prefix で
    // 外部プロセスを起こさない、という既存の順序）。
    let (status, body) = send_request(
        app.clone(),
        "POST",
        "/api/agents/a1/nostr/generate",
        Some(serde_json::json!({"prefix": "bbb"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "prefix の検証が先（bech32 に無い文字）: {body}"
    );
}

/// #291: 対話ターンでは evaluator を呼ばない。
///
/// 撤去前は「契約付き active タスクがあり、その run がツールを使った」場合に、
/// 生成 run の直後で毎回 evaluator を 1 回まわし、採点結果を `session_logs`
/// (log_type=evaluation) と台帳 progress に書いていた。会話再構築でその行が
/// 「次ターンでギャップを埋めろ」という指示文つきで復元され、直前のユーザー
/// 発言より採点の圧が勝つ事故（#291）につながった。
///
/// このテストは旧実装の**発火条件をすべて満たした**うえで、
/// - LLM 呼び出しが生成 run のぶん（2 回）だけであること
/// - `evaluation` ログも `[evaluation]` 進捗も残らないこと
/// を確かめる。旧実装ならモックのキューが尽き（3 回目を要求し）、記録も残る。
#[tokio::test]
async fn test_dialogue_turn_does_not_invoke_evaluator() {
    let (app, db, mock) = create_test_app_with_llm();

    let (agent_a, app) = create_test_agent_named(app, "Asker", "Curious Mind").await;
    let (agent_b, app) = create_test_agent_named(app, "Worker", "Diligent Hand").await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "契約付きタスクの遂行",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // 旧 verify 段の発火条件その 1: 応答側に contract 非空の active タスクがある。
    let task_id = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::insert_task_ledger(
            &conn,
            &agent_b,
            &session_id,
            "スキルを 1 つ作る",
            Some("learn_from_experience で新しいスキルが DB に入っていること"),
        )
        .unwrap()
    };

    // 発火条件その 2: この run が実際にツールを実行する。
    // キューは生成 run のぶん（tool_call → text）だけ積む。
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-291".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "contract_work",
                "description": "契約付きタスクを進める",
                "situation_pattern": "when a contract task is active",
                "guidance": "証拠を残しながら進める"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("スキルを作りました。");

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "契約どおり進めてほしい"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0]["tool_calls_made"].as_i64().unwrap(),
        1,
        "テストの前提: 旧 verify 段の発火条件（ツールを使った run）を満たすこと"
    );

    // 生成 run の 2 回だけ。3 回目があれば evaluator がまだ対話ターンにいる。
    let calls = mock.system_prompts();
    assert_eq!(
        calls.len(),
        2,
        "対話ターンで余計な LLM 呼び出しが走っている（evaluator の残留）: {calls:#?}"
    );

    let conn = db.lock().unwrap();
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap();
    assert!(
        logs.iter().all(|l| l.log_type != "evaluation"),
        "対話ターンが evaluation ログを書いている: {:#?}",
        logs.iter().map(|l| &l.log_type).collect::<Vec<_>>()
    );

    let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 50).unwrap();
    assert!(
        progress.iter().all(|p| !p.content.contains("[evaluation]")),
        "対話ターンが台帳へ採点を書いている: {progress:#?}"
    );
}

// ==================== #412: context_window 未登録モデルは設定時に弾く ====================

/// `model_pricing` に行を入れる唯一の経路。ここが無かったので誰も入れられず、
/// 空でも既定値で黙って動いていた。
#[tokio::test]
async fn test_model_pricing_put_then_list() {
    let app = create_test_app();

    let (status, resp) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": "testprov",
            "model": "testmodel",
            "input_price_per_1m": 1.5,
            "output_price_per_1m": 3.0,
            "context_window": 200000
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["saved"], true);

    let (status, resp) = send_request(app, "GET", "/api/llm/model-pricing", None).await;
    assert_eq!(status, StatusCode::OK);
    let models = resp["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["provider"], "testprov");
    assert_eq!(models[0]["context_window"], 200000);
}

/// `context_window` こそが登録の目的なので、0 以下は受け付けない。
/// （通してしまうと「登録済みなのに予算が決まらない」行が作れる）
#[tokio::test]
async fn test_model_pricing_rejects_non_positive_context_window() {
    let app = create_test_app();
    let (status, _) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": "testprov", "model": "testmodel", "context_window": 0
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 弾いた以上、行は作られていない。
    let (_, resp) = send_request(app, "GET", "/api/llm/model-pricing", None).await;
    assert!(resp["models"].as_array().unwrap().is_empty());
}

async fn register_model(app: Router, provider: &str, model: &str, window: i64) {
    // #676: テストの router は空でプロバイダ能力が既定（送る＝登録必須）に倒れるため、
    // 「完全登録」を表すには max_output_tokens も入れる（context_window だけではモデル変更
    // ゲートを通らない）。ゲートの案Y 条件分岐は core の単体テストで担保する。
    let (status, _) = send_request(
        app,
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": provider, "model": model,
            "context_window": window, "max_output_tokens": 8192
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn agent_model(app: Router, agent_id: &str) -> serde_json::Value {
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    resp["model"].clone()
}

#[tokio::test]
async fn test_patch_agent_rejects_unregistered_model() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:unregistered"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["updated"], false);
    let err = resp["error"].as_str().unwrap();
    assert!(err.contains("model_pricing"), "{err}");
    assert!(err.contains("/api/llm/model-pricing"), "{err}");

    // 拒否した設定は保存されていない。
    assert_eq!(agent_model(app, &agent_id).await, serde_json::Value::Null);
}

#[tokio::test]
async fn test_patch_agent_accepts_registered_model() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    register_model(app.clone(), "testprov", "testmodel", 200_000).await;

    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:testmodel"})),
    )
    .await;
    assert_eq!(resp["updated"], true);
    assert_eq!(agent_model(app, &agent_id).await, "testprov:testmodel");
}

/// グローバル既定へ戻す操作は検証の対象外。既定側は config のホットリロードで
/// 検証するので、ここで塞ぐと戻せなくなる。
///
/// クリアの表現は**空文字**。serde の `Option<Option<_>>` は JSON null を
/// 「変更なし」に潰すため（`apply_agent_patch` の reasoning_effort に同趣旨のコメント）。
#[tokio::test]
async fn test_patch_agent_can_clear_model_without_registration() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    register_model(app.clone(), "testprov", "testmodel", 200_000).await;
    send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:testmodel"})),
    )
    .await;

    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": ""})),
    )
    .await;
    assert_eq!(resp["updated"], true);
    assert_eq!(agent_model(app, &agent_id).await, "");
}

#[tokio::test]
async fn test_put_agent_rejects_unregistered_model() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Test Agent",
            "persona_name": "TestPersona",
            "model": "testprov:unregistered"
        })),
    )
    .await;
    assert_eq!(resp["updated"], false);
    assert!(resp["error"].as_str().unwrap().contains("model_pricing"));
}

/// **既存の設定を壊さない。** 登録が始まる前から `agents.model` に入っていた値は、
/// そのまま送り直す限り弾かれない（識別情報だけを編集する PUT が通ること）。
/// 検証が効くのは**新しく設定するとき**だけ。
#[tokio::test]
async fn test_put_agent_keeps_existing_unregistered_model() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;

    // 検証が入る前の状態を再現: 未登録モデルを API を経由せず直接書き込む。
    {
        let conn = db.lock().unwrap();
        let mut row = opencrab_db::queries::get_agent(&conn, &agent_id)
            .unwrap()
            .unwrap();
        row.model = Some("testprov:legacy".to_string());
        opencrab_db::queries::upsert_agent(&conn, &row).unwrap();
    }

    let (_, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Renamed",
            "persona_name": "TestPersona",
            "model": "testprov:legacy"
        })),
    )
    .await;
    assert_eq!(resp["updated"], true, "{resp}");
    assert_eq!(agent_model(app, &agent_id).await, "testprov:legacy");
}

/// 上と同じ状況から**別の未登録モデルへ移す**のは弾く。
/// 「既存値は素通し」が「未登録なら何でも通る」に化けていないこと。
#[tokio::test]
async fn test_put_agent_rejects_switching_to_another_unregistered_model() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    {
        let conn = db.lock().unwrap();
        let mut row = opencrab_db::queries::get_agent(&conn, &agent_id)
            .unwrap()
            .unwrap();
        row.model = Some("testprov:legacy".to_string());
        opencrab_db::queries::upsert_agent(&conn, &row).unwrap();
    }

    let (_, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Renamed",
            "persona_name": "TestPersona",
            "model": "testprov:another"
        })),
    )
    .await;
    assert_eq!(resp["updated"], false);
    assert_eq!(agent_model(app, &agent_id).await, "testprov:legacy");
}

/// 投入 API は provider/model を trim して保存し、gate も同じ正規化で引く。
/// 揃っていないと「登録したのに未登録と言われる」になる。
#[tokio::test]
async fn test_model_pricing_trim_is_consistent_between_put_and_gate() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, _) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": "  testprov  ", "model": "  testmodel  ",
            "context_window": 200000, "max_output_tokens": 8192
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 空白なしの spec で通る（保存側が trim されている）。testprov は空 router で「送る」
    // 既定に倒れるため、モデル変更ゲートは max_output_tokens も要求する（#676 案Y）。上で登録済み。
    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:testmodel"})),
    )
    .await;
    assert_eq!(resp["updated"], true, "{resp}");

    // 空白入りの spec でも通る（参照側も trim されている）。
    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": " testprov : testmodel "})),
    )
    .await;
    assert_eq!(resp["updated"], true, "{resp}");
}
