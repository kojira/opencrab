fn insert_speech(db: &opencrab_db::Db, agent_id: &str, session_id: &str, content: &str) {
    let conn = db.lock().unwrap();
    opencrab_db::queries::insert_session_log(
        &conn,
        &opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            log_type: "speech".into(),
            content: content.into(),
            speaker_id: Some(agent_id.into()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap();
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

    let (status, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
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

    let (status, _) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_id,
            "content": "Hello world"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_owner_instruction_missing_session_is_404() {
    let app = create_test_app();
    let (status, resp) = send_request(
        app,
        "POST",
        "/api/sessions/no-such-session/owner",
        Some(serde_json::json!({"content": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = resp;
}

#[tokio::test]
async fn test_owner_instruction_records_without_llm() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Owner Record",
            "participant_ids": [&agent_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/owner"),
        Some(serde_json::json!({"content": "owner says hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = resp;
}

#[tokio::test]
async fn test_owner_instruction_triggers_turn_and_llm_log() {
    let (app, db, mock) = create_test_app_with_llm();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "Owner Turn",
            "participant_ids": [&agent_id]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    mock.push_text_response("owner turn reply");

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/owner"),
        Some(serde_json::json!({"content": "please respond"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = (resp, agent_id, db, session_id);
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
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;

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
    insert_speech(&db, &agent_id, &session_id, "Rust programming is fun");
    insert_speech(&db, &agent_id, &session_id, "Python is also great");

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
    let (app, db) = create_test_app_with_db();

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

    for content in &[
        "The architecture of OpenCrab is modular",
        "Each agent has a soul and identity",
        "Skills can be acquired at runtime",
    ] {
        insert_speech(&db, &agent_id, &session_id, content);
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

    // 9. Verify get is 200 null
    let (status, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
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

