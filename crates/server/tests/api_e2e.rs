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
    let conn = opencrab_db::init_memory().unwrap();
    let state = AppState {
        db: opencrab_db::Db::from_connection(conn),
        llm_router: opencrab_server::SharedLlmRouter::new(LlmRouter::new()),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        default_model: "mock:test".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        #[cfg(feature = "discord")]
        discord_manager: None,
        nostr_manager: None,
        mcp_manager: None,
        web_gateway: std::sync::Arc::new(opencrab_server::web_gateway::WebGateway::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
    };
    create_router(state)
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
    assert!(resp.as_array().unwrap().len() >= 1);
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

// ==================== MockLlmProvider ====================

/// A mock LLM provider that returns pre-queued responses.
struct MockLlmProvider {
    responses: Mutex<VecDeque<ChatResponse>>,
}

impl MockLlmProvider {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
        }
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

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
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
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir()
            .join("opencrab_test")
            .to_string_lossy()
            .to_string(),
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        #[cfg(feature = "discord")]
        discord_manager: None,
        nostr_manager: None,
        mcp_manager: None,
        web_gateway: std::sync::Arc::new(opencrab_server::web_gateway::WebGateway::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
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
        opencrab_db::queries::list_skills(&conn, &responses[0]["agent_id"].as_str().unwrap(), false)
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
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: cfg,
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        #[cfg(feature = "discord")]
        discord_manager: None,
        nostr_manager: None,
        mcp_manager: None,
        web_gateway: std::sync::Arc::new(opencrab_server::web_gateway::WebGateway::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
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
            started_at: std::time::Instant::now(),
            reply_target: None,
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
}
