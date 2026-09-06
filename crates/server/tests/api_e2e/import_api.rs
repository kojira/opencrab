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

