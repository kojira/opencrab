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
        db: Arc::new(Mutex::new(conn)),
        llm_router: Arc::new(LlmRouter::new()),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
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
        "DELETE" => builder.method("DELETE"),
        _ => panic!("unsupported method"),
    };

    if method == "POST" || method == "PUT" {
        builder = builder.header("content-type", "application/json");
    }

    let req = builder.body(body).unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .unwrap_or(serde_json::json!(body_bytes.to_vec()));
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
            "persona_name": "TestPersona",
            "role": "discussant"
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

    let (status, resp) =
        send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["identity"]["name"], "Test Agent");
    assert_eq!(resp["soul"]["persona_name"], "TestPersona");
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
    let (_, resp) =
        send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp["identity"].is_null());
}

#[tokio::test]
async fn test_update_soul() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let soul_update = serde_json::json!({
        "agent_id": agent_id,
        "persona_name": "UpdatedPersona",
        "social_style_json": "{}",
        "personality_json": "{}",
        "thinking_style_json": "{}",
        "custom_traits_json": null
    });

    let (status, _) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/soul"),
        Some(soul_update),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, resp) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/soul"),
        None,
    )
    .await;
    assert_eq!(resp["persona_name"], "UpdatedPersona");
}

#[tokio::test]
async fn test_update_identity() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let identity_update = serde_json::json!({
        "agent_id": agent_id,
        "name": "Updated Name",
        "role": "facilitator",
        "job_title": "Lead",
        "organization": "OpenCrab Inc",
        "image_url": null,
        "metadata_json": null
    });

    let (status, _) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/identity"),
        Some(identity_update),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, resp) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/identity"),
        None,
    )
    .await;
    assert_eq!(resp["name"], "Updated Name");
    assert_eq!(resp["role"], "facilitator");
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

    let (_, resp) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/skills"),
        None,
    )
    .await;
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
    let (_, resp) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/skills"),
        None,
    )
    .await;
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
    assert_eq!(resp.as_array().unwrap().len(), 0);
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
    let (_, resp) =
        send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["identity"]["name"], "Workflow Agent");
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
            "persona_name": "CRUD Persona",
            "role": "discussant"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let agent_id = resp["id"].as_str().unwrap().to_string();

    // 2. Read - verify created
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["identity"]["name"], "CRUD Agent");
    assert_eq!(resp["identity"]["role"], "discussant");
    assert_eq!(resp["soul"]["persona_name"], "CRUD Persona");

    // 3. Update identity
    let (status, _) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/identity"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "name": "Updated CRUD Agent",
            "role": "facilitator",
            "job_title": "Team Lead",
            "organization": "OpenCrab Labs",
            "image_url": null,
            "metadata_json": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 4. Update soul
    let (status, _) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/soul"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "persona_name": "Updated CRUD Persona",
            "social_style_json": r#"{"style":"driver"}"#,
            "personality_json": r#"{"openness":0.8}"#,
            "thinking_style_json": r#"{"primary":"Creative"}"#,
            "custom_traits_json": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 5. Read - verify both updates
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["identity"]["name"], "Updated CRUD Agent");
    assert_eq!(resp["identity"]["role"], "facilitator");
    assert_eq!(resp["identity"]["job_title"], "Team Lead");
    assert_eq!(resp["identity"]["organization"], "OpenCrab Labs");
    assert_eq!(resp["soul"]["persona_name"], "Updated CRUD Persona");

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
    let (_, resp) =
        send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp["identity"].is_null());
}

#[tokio::test]
async fn test_create_agent_minimal_fields() {
    let app = create_test_app();

    // Create with only name (no role)
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

    // Should default to "discussant"
    let (_, resp) =
        send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["identity"]["role"], "discussant");
}

#[tokio::test]
async fn test_delete_nonexistent_agent() {
    let app = create_test_app();

    let (status, resp) = send_request(
        app,
        "DELETE",
        "/api/agents/nonexistent-id-12345",
        None,
    )
    .await;
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

    async fn available_models(
        &self,
    ) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
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
/// Returns (Router, Arc<Mutex<Connection>>, Arc<MockLlmProvider>).
fn create_test_app_with_llm() -> (Router, Arc<Mutex<rusqlite::Connection>>, Arc<MockLlmProvider>) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = Arc::new(Mutex::new(conn));

    let mock = Arc::new(MockLlmProvider::new());
    let mut router = LlmRouter::new();
    router.add_provider(mock.clone() as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");

    let state = AppState {
        db: db.clone(),
        llm_router: Arc::new(router),
        workspace_base: std::env::temp_dir()
            .join("opencrab_test")
            .to_string_lossy()
            .to_string(),
    };
    let app = create_router(state);
    (app, db, mock)
}

/// Create a named agent with a specific persona via the API.
async fn create_test_agent_named(
    app: Router,
    name: &str,
    persona: &str,
) -> (String, Router) {
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": name,
            "persona_name": persona,
            "role": "discussant"
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

    let skill = skills
        .iter()
        .find(|s| s.name == "collaborative_learning");
    assert!(
        skill.is_some(),
        "Should find 'collaborative_learning' skill"
    );
    let skill = skill.unwrap();
    assert_eq!(skill.source_type, "experience");
    assert!(skill.is_active);
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
