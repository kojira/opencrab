//! Integration test: SkillEngine → BridgedExecutor → ActionDispatcher → real Actions
//!
//! Uses MockLlm (no API keys needed). Validates that the SkillEngine can
//! drive real actions (search history, create skills, learn from experience)
//! through the BridgedExecutor bridge.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_actions::bridge::BridgedExecutor;
use opencrab_actions::dispatcher::ActionDispatcher;
use opencrab_actions::traits::ActionContext;
use opencrab_core::{
    ChatRequestSimple, ChatResponseSimple, LlmClient, SkillEngine, ToolCall,
};

// ---------------------------------------------------------------------------
// MockLlm — returns pre-queued responses in order
// ---------------------------------------------------------------------------

struct MockLlm {
    responses: Mutex<Vec<ChatResponseSimple>>,
}

impl MockLlm {
    fn new(responses: Vec<ChatResponseSimple>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn chat(&self, _req: ChatRequestSimple) -> anyhow::Result<ChatResponseSimple> {
        let mut rs = self.responses.lock().unwrap();
        if rs.is_empty() {
            anyhow::bail!("MockLlm: no more responses");
        }
        Ok(rs.remove(0))
    }
}

// ---------------------------------------------------------------------------
// Helper: create a BridgedExecutor backed by in-memory DB
// ---------------------------------------------------------------------------

fn setup() -> (tempfile::TempDir, BridgedExecutor) {
    let conn = opencrab_db::init_memory().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();

    let ctx = ActionContext {
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: Arc::new(Mutex::new(conn)),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(Mutex::new(None)),
        model_override: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
        gateway_admin: None,
    };

    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    (dir, executor)
}

fn setup_with_data() -> (tempfile::TempDir, BridgedExecutor, Arc<Mutex<rusqlite::Connection>>) {
    let conn = opencrab_db::init_memory().unwrap();

    // Seed a session log so search_my_history can find it
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        log_type: "message".to_string(),
        content: "Rust programming is wonderful".to_string(),
        speaker_id: Some("agent-1".to_string()),
        turn_number: Some(1),
        metadata_json: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &log).unwrap();

    let db = Arc::new(Mutex::new(conn));
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();

    let ctx = ActionContext {
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: Arc::clone(&db),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(Mutex::new(None)),
        model_override: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
        gateway_admin: None,
    };

    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    (dir, executor, db)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Engine calls search_my_history → create_my_skill → returns text.
#[tokio::test]
async fn test_engine_search_then_create_skill() {
    let (_dir, executor, db) = setup_with_data();

    let llm = MockLlm::new(vec![
        // Step 1: LLM calls search_my_history
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "search_my_history".to_string(),
                arguments: serde_json::json!({"query": "Rust"}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 2: LLM calls create_my_skill based on search results
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-2".to_string(),
                name: "create_my_skill".to_string(),
                arguments: serde_json::json!({
                    "name": "Rust Expertise",
                    "description": "Knowledge about Rust programming",
                    "situation_pattern": "when discussing Rust",
                    "guidance": "Share detailed Rust knowledge"
                }),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        // Step 3: Final text response
        ChatResponseSimple {
            content: Some(
                "I searched my history and created a new skill based on my Rust knowledge."
                    .to_string(),
            ),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run(
            "You are a learning agent",
            "Review your history and create a skill",
            "mock-model",
        )
        .await
        .unwrap();

    assert_eq!(result.iterations, 3);
    assert_eq!(result.tool_calls_made, 2);
    assert!(!result.stopped_by_limit);
    assert!(result.response.contains("skill"));

    // Verify the skill was actually persisted in the DB
    let conn = db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills(&conn, "agent-1", false).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "Rust Expertise");
    assert_eq!(skills[0].source_type, "self_created");
}

/// Engine calls learn_from_experience → returns text. Verify DB skill insertion.
#[tokio::test]
async fn test_engine_learn_from_experience() {
    let (_dir, executor, db) = setup_with_data();

    let llm = MockLlm::new(vec![
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "learn_from_experience".to_string(),
                arguments: serde_json::json!({
                    "experience": "Helped user debug a complex issue",
                    "outcome": "success",
                    "lesson": "Ask for error messages first",
                    "skill_name": "debug_workflow",
                    "situation_pattern": "when user reports a bug",
                    "guidance": "Request stack trace before suggesting fixes"
                }),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        ChatResponseSimple {
            content: Some("I've learned a new debugging workflow skill.".to_string()),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run("You are a learning agent", "Learn from this", "mock-model")
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);
    assert_eq!(result.tool_calls_made, 1);

    // Verify the skill was persisted
    let conn = db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills(&conn, "agent-1", false).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "debug_workflow");
    assert_eq!(skills[0].source_type, "experience");
}

/// BridgedExecutor.list_tools() returns all registered actions.
#[tokio::test]
async fn test_engine_lists_all_tools() {
    let (_dir, executor) = setup();

    use opencrab_core::ActionExecutor;
    let tools = executor.list_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    assert!(names.contains(&"search_my_history"), "missing search_my_history");
    assert!(names.contains(&"create_my_skill"), "missing create_my_skill");
    assert!(
        names.contains(&"learn_from_experience"),
        "missing learn_from_experience"
    );
    assert!(names.contains(&"learn_from_peer"), "missing learn_from_peer");
    assert!(
        names.contains(&"reflect_and_learn"),
        "missing reflect_and_learn"
    );
    assert!(names.contains(&"send_speech"), "missing send_speech");
    assert!(names.contains(&"ws_read"), "missing ws_read");
    assert!(tools.len() >= 18, "expected 18+ tools, got {}", tools.len());
}

/// Unknown action returns error result, engine continues and produces final text.
#[tokio::test]
async fn test_engine_unknown_action_handled() {
    let (_dir, executor) = setup();

    let llm = MockLlm::new(vec![
        ChatResponseSimple {
            content: None,
            tool_calls: vec![ToolCall {
                id: "tc-1".to_string(),
                name: "nonexistent_action".to_string(),
                arguments: serde_json::json!({}),
            }],
            finish_reason: "tool_calls".to_string(),
            usage: None,
        },
        ChatResponseSimple {
            content: Some("That action was not found.".to_string()),
            tool_calls: vec![],
            finish_reason: "stop".to_string(),
            usage: None,
        },
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    let result = engine
        .run("system", "try something", "mock-model")
        .await
        .unwrap();

    assert_eq!(result.tool_calls_made, 1);
    assert!(!result.stopped_by_limit);
}
