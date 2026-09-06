// ==================== LLM-integrated E2E Tests ====================

/// Test: Agent A sends a message → Agent B responds via SkillEngine.
#[tokio::test]
#[ignore = "old HTTP conversation route removed"]
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
#[ignore = "old HTTP conversation route removed"]
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
#[ignore = "old HTTP conversation route removed"]
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
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = resp;
}

