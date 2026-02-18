//! End-to-end tests with REAL LLM (OpenRouter).
//!
//! These tests call actual LLM APIs and require `OPENROUTER_API_KEY`.
//! They are `#[ignore]` so `cargo test` won't run them by default.
//!
//! Run with:
//!   OPENROUTER_API_KEY="sk-or-..." cargo test -p opencrab-server --test real_llm_e2e -- --ignored --nocapture

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

use opencrab_llm::providers::openrouter::OpenRouterProvider;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;
use opencrab_server::{create_router, AppState};

// ==================== Helpers ====================

fn api_key() -> String {
    std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set")
}

/// Create a test app backed by a real OpenRouter LLM provider.
fn create_real_llm_app() -> (Router, Arc<Mutex<rusqlite::Connection>>) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = Arc::new(Mutex::new(conn));

    let provider = OpenRouterProvider::new(api_key()).with_title("OpenCrab E2E Test");

    let mut router = LlmRouter::new();
    router.add_provider(Arc::new(provider) as Arc<dyn LlmProvider>);
    router.set_default_provider("openrouter");
    // Map "default" model to a cheap, fast model with good function calling
    router.add_model_mapping("default", "openrouter:openai/gpt-4o-mini");

    let workspace_base = std::env::temp_dir()
        .join("opencrab_real_llm_test")
        .to_string_lossy()
        .to_string();

    let state = AppState {
        db: db.clone(),
        llm_router: Arc::new(router),
        workspace_base,
    };
    let app = create_router(state);
    (app, db)
}

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

/// Create agent with a custom personality (stored in personality_json).
async fn create_agent_with_personality(
    app: Router,
    name: &str,
    persona: &str,
    personality: &str,
) -> (String, Router) {
    // Create agent
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

    // Update soul with personality that encourages tool usage
    send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/soul"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "persona_name": persona,
            "social_style_json": "{}",
            "personality_json": personality,
            "thinking_style_json": "{}",
            "custom_traits_json": null
        })),
    )
    .await;

    (agent_id, app)
}

// ==================== Real LLM E2E Tests ====================

/// Scenario 1: Basic two-agent conversation via SkillEngine.
///
/// Alice sends a message → Bob (real LLM) generates a thoughtful response.
/// Verifies the full pipeline: HTTP → send_message → SkillEngine → LLM → response.
#[tokio::test]
#[ignore]
async fn test_real_llm_basic_conversation() {
    let (app, _db) = create_real_llm_app();

    let (agent_a, app) =
        create_agent_with_personality(app, "Alice", "Curious Researcher", r#"{"trait":"curious","style":"asks questions"}"#).await;
    let (agent_b, app) =
        create_agent_with_personality(app, "Bob", "Thoughtful Analyst", r#"{"trait":"analytical","style":"gives detailed answers"}"#).await;

    // Create session
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "The future of autonomous AI agents",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Alice sends a message → Bob responds via real LLM
    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "What do you think are the most important capabilities for autonomous AI agents? Please share your perspective in 2-3 sentences."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1, "Bob should respond");

    let bob_response = responses[0]["content"].as_str().unwrap();
    println!("\n[Alice]: What do you think are the most important capabilities for autonomous AI agents?");
    println!("[Bob]: {}\n", bob_response);

    assert!(
        bob_response.len() > 20,
        "Bob's response should be substantive, got: {bob_response}"
    );
    assert!(
        !bob_response.contains("Error"),
        "Response should not be an error"
    );
}

/// Scenario 2: Agent learns from discussion experience and creates a skill.
///
/// The Learner agent's personality explicitly says to use `learn_from_experience`
/// after receiving insights. This tests the full tool-calling flow with a real LLM.
#[tokio::test]
#[ignore]
async fn test_real_llm_agent_learns_and_creates_skill() {
    let (app, db) = create_real_llm_app();

    // Teacher: just talks normally
    let (teacher_id, app) = create_agent_with_personality(
        app,
        "Teacher",
        "Expert Mentor",
        r#"{"trait":"knowledgeable","teaching_style":"clear and structured"}"#,
    )
    .await;

    // Learner: personality strongly encourages tool use
    let learner_personality = serde_json::json!({
        "trait": "self-improving",
        "learning_style": "reflective",
        "IMPORTANT_INSTRUCTION": "After receiving valuable information in a discussion, you MUST call the learn_from_experience tool to record what you learned as a reusable skill. Always extract a named skill from the conversation."
    })
    .to_string();

    let (learner_id, app) = create_agent_with_personality(
        app,
        "Learner",
        "Self-Improving Agent",
        &learner_personality,
    )
    .await;

    // Create session
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Teaching effective debugging techniques",
            "participant_ids": [&teacher_id, &learner_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Teacher sends a lesson → Learner should learn and call tools
    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": teacher_id,
            "content": "Here is an important debugging technique: When you encounter a bug, first reproduce it consistently, then isolate the minimal reproduction case, check the input/output boundaries, and add logging around the suspicious area. This systematic approach saves hours of random guessing. Now, please use your learn_from_experience tool to record this as a skill."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1, "Learner should respond");

    let learner_response = &responses[0];
    let content = learner_response["content"].as_str().unwrap();
    let tool_calls_made = learner_response["tool_calls_made"].as_i64().unwrap();

    println!("\n[Teacher]: (debugging technique lesson)");
    println!("[Learner]: {}", content);
    println!("  → tool_calls_made: {}\n", tool_calls_made);

    // Check if skills were created in DB
    let skills = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_skills(&conn, &learner_id, false).unwrap()
    };

    println!("Skills created by Learner: {}", skills.len());
    for skill in &skills {
        println!(
            "  - {} (source: {}, active: {})",
            skill.name, skill.source_type, skill.is_active
        );
        println!("    description: {}", skill.description);
        println!("    guidance: {}", skill.guidance);
    }

    if tool_calls_made > 0 {
        assert!(
            !skills.is_empty(),
            "Learner used tools but no skills were created"
        );
        println!("\n✓ Learner successfully created {} skill(s) via real LLM!", skills.len());
    } else {
        println!("\n⚠ Learner didn't use tools this time (LLM chose text-only response)");
        println!("  This can happen — real LLMs are non-deterministic.");
        println!("  The response was still valid: {}", content);
    }
}

/// Scenario 3: Multi-round discussion where agents build on each other's ideas.
///
/// Two agents discuss over 3 rounds. After the discussion, the Reflector
/// agent should use `reflect_and_learn` or `learn_from_experience` to capture insights.
#[tokio::test]
#[ignore]
async fn test_real_llm_multi_round_discussion_with_reflection() {
    let (app, db) = create_real_llm_app();

    let debater_personality = serde_json::json!({
        "trait": "opinionated",
        "style": "makes strong arguments with examples",
        "brevity": "keep responses to 2-3 sentences"
    })
    .to_string();

    let reflector_personality = serde_json::json!({
        "trait": "reflective and self-improving",
        "style": "thoughtful, synthesizes different viewpoints",
        "brevity": "keep responses to 2-3 sentences",
        "IMPORTANT_INSTRUCTION": "In the final round of a discussion, you MUST call either learn_from_experience or reflect_and_learn to capture your insights from the discussion as a permanent skill or memory."
    })
    .to_string();

    let (debater_id, app) = create_agent_with_personality(
        app,
        "Debater",
        "Strong Advocate",
        &debater_personality,
    )
    .await;

    let (reflector_id, app) = create_agent_with_personality(
        app,
        "Reflector",
        "Thoughtful Synthesizer",
        &reflector_personality,
    )
    .await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Should AI agents be able to modify their own code?",
            "participant_ids": [&debater_id, &reflector_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    let sep = "=".repeat(60);
    println!("\n{sep}");
    println!("DISCUSSION: Should AI agents be able to modify their own code?");
    println!("{sep}\n");

    // Round 1: Debater opens
    println!("--- Round 1 ---\n");
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": debater_id,
            "content": "I believe AI agents should absolutely be able to modify their own code. Self-improvement is the key to reaching truly autonomous intelligence. What do you think?"
        })),
    )
    .await;

    let responses = resp["responses"].as_array().unwrap();
    if !responses.is_empty() {
        println!("[Debater]: I believe AI agents should be able to modify their own code...");
        println!("[Reflector]: {}\n", responses[0]["content"].as_str().unwrap_or("(no response)"));
    }

    // Round 2: Reflector pushes back, Debater responds
    println!("--- Round 2 ---\n");
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": reflector_id,
            "content": "While self-improvement sounds appealing, uncontrolled self-modification could lead to unpredictable behavior. We need safety constraints. How do you address alignment risks?"
        })),
    )
    .await;

    let responses = resp["responses"].as_array().unwrap();
    if !responses.is_empty() {
        println!("[Reflector]: While self-improvement sounds appealing...");
        println!("[Debater]: {}\n", responses[0]["content"].as_str().unwrap_or("(no response)"));
    }

    // Round 3: Final round — Debater sends closing argument,
    // Reflector should reflect and learn
    println!("--- Round 3 (Final — Reflector should use tools) ---\n");
    let (_, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": debater_id,
            "content": "You raise valid concerns about safety. I agree we need sandboxed environments and formal verification. Self-modification within guardrails is the middle ground. This has been a very productive discussion — I encourage you to use your tools to record what you've learned from this exchange."
        })),
    )
    .await;

    let responses = resp["responses"].as_array().unwrap();
    if !responses.is_empty() {
        let reflector_resp = &responses[0];
        let content = reflector_resp["content"].as_str().unwrap_or("(no response)");
        let tool_calls = reflector_resp["tool_calls_made"].as_i64().unwrap_or(0);
        println!("[Debater]: (closing argument + encouragement to use tools)");
        println!("[Reflector]: {}", content);
        println!("  → tool_calls_made: {}\n", tool_calls);
    }

    // Check what the agents created
    let (skills_reflector, memories_reflector) = {
        let conn = db.lock().unwrap();
        let skills = opencrab_db::queries::list_skills(&conn, &reflector_id, false).unwrap();
        let memories =
            opencrab_db::queries::list_curated_memories(&conn, &reflector_id).unwrap();
        (skills, memories)
    };

    println!("{sep}");
    println!("RESULTS:");
    println!("  Reflector skills: {}", skills_reflector.len());
    for s in &skills_reflector {
        println!("    - {} (type: {}, active: {})", s.name, s.source_type, s.is_active);
        if !s.description.is_empty() {
            println!("      {}", s.description);
        }
    }
    println!("  Reflector memories: {}", memories_reflector.len());
    for m in &memories_reflector {
        println!("    - [{}] {}", m.category, &m.content[..m.content.len().min(80)]);
    }

    // Verify conversation was logged
    let session_logs = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap()
    };
    println!("  Session logs: {}", session_logs.len());
    assert!(
        session_logs.len() >= 3,
        "Should have at least 3 messages in session log (got {})",
        session_logs.len()
    );

    if !skills_reflector.is_empty() || !memories_reflector.is_empty() {
        println!(
            "\n✓ Reflector captured insights! {} skills, {} memories",
            skills_reflector.len(),
            memories_reflector.len()
        );
    } else {
        println!("\n⚠ Reflector didn't capture insights via tools this time.");
        println!("  This can happen with real LLMs — they don't always follow tool-use hints.");
    }
    println!("{sep}");
}

/// Scenario 4: Agent searches its own history and creates a skill from findings.
///
/// First seeds conversation history, then prompts the agent to search and learn.
#[tokio::test]
#[ignore]
async fn test_real_llm_search_history_and_create_skill() {
    let (app, db) = create_real_llm_app();

    let researcher_personality = serde_json::json!({
        "trait": "meticulous researcher",
        "style": "data-driven, evidence-based",
        "IMPORTANT_INSTRUCTION": "When asked to review your history, you MUST: 1) Call search_my_history to find relevant past discussions, 2) Based on what you find, call learn_from_experience or create_my_skill to capture a reusable skill. Always use both tools."
    })
    .to_string();

    let (researcher_id, app) = create_agent_with_personality(
        app,
        "Researcher",
        "Evidence-Based Thinker",
        &researcher_personality,
    )
    .await;

    let (prompter_id, app) = create_agent_with_personality(
        app,
        "Prompter",
        "Discussion Partner",
        r#"{"trait":"helpful"}"#,
    )
    .await;

    // Create session and seed some conversation history
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Effective collaboration patterns",
            "participant_ids": [&researcher_id, &prompter_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Seed history: several messages about collaboration
    let seed_messages = [
        (&researcher_id, "I've found that pair programming significantly reduces bugs in complex codebases."),
        (&prompter_id, "That's interesting. What about code reviews?"),
        (&researcher_id, "Code reviews are also effective, especially when reviewers focus on design patterns rather than style nits."),
        (&prompter_id, "How do you combine both approaches?"),
        (&researcher_id, "The best teams I've seen use pair programming for complex features and async code reviews for smaller changes. This balances thoroughness with velocity."),
    ];

    for (agent_id, content) in &seed_messages {
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

    println!("\n--- Seeded {} messages about collaboration ---\n", seed_messages.len());

    // Now Prompter asks Researcher to review and learn
    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": prompter_id,
            "content": "You've shared great insights about collaboration. Please review your discussion history by searching for 'programming' or 'code review' using your search_my_history tool, then create a skill from what you've learned using learn_from_experience."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1, "Researcher should respond");

    let researcher_resp = &responses[0];
    let content = researcher_resp["content"].as_str().unwrap();
    let tool_calls = researcher_resp["tool_calls_made"].as_i64().unwrap();

    println!("[Prompter]: Please review your history and create a skill...");
    println!("[Researcher]: {}", content);
    println!("  → tool_calls_made: {}\n", tool_calls);

    // Check results
    let skills = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_skills(&conn, &researcher_id, false).unwrap()
    };

    println!("Skills created by Researcher: {}", skills.len());
    for skill in &skills {
        println!("  - {} (source: {})", skill.name, skill.source_type);
        println!("    description: {}", skill.description);
        println!("    pattern: {}", skill.situation_pattern);
        println!("    guidance: {}", skill.guidance);
    }

    if tool_calls > 0 {
        println!("\n✓ Researcher used {} tool call(s)!", tool_calls);
        if !skills.is_empty() {
            println!("✓ Created {} skill(s) from history review!", skills.len());
        }
    } else {
        println!("\n⚠ Researcher chose a text-only response (no tool calls).");
    }
}
