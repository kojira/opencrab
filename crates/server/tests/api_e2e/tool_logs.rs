/// 載せ替え工程 5-b: `GET /api/agents/{id}/tool-logs` は llm-logs と同型の配列。
/// done / failed / refused の 3 態が行になる。
#[tokio::test]
async fn test_tool_logs_api_lists_done_failed_refused() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, body) = send_request(
        app.clone(),
        "GET",
        &format!("/api/agents/{agent_id}/tool-logs?limit=20"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));

    {
        let conn = db.lock().unwrap();
        for (name, outcome, text) in [
            ("search_my_history", "done", r#"{"hits":1}"#),
            ("nonexistent_tool", "failed", "Unknown action"),
            ("execute_shell", "refused", "rejected: owner"),
        ] {
            opencrab_db::queries::insert_tool_log(
                &conn,
                &opencrab_db::queries::ToolLogWrite {
                    agent_id: agent_id.clone(),
                    session_id: Some("session-1".into()),
                    tool_name: name.into(),
                    args_json: "{}".into(),
                    outcome: outcome.into(),
                    result_text: text.into(),
                    started_at: Some("2026-08-25T00:00:00Z".into()),
                    latency_ms: Some(5),
                    iteration: None,
                },
            )
            .unwrap();
        }
    }

    let (status, body) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/tool-logs?limit=20"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("llm-logs と同型の配列");
    assert_eq!(rows.len(), 3);
    let outcomes: Vec<&str> = rows
        .iter()
        .map(|r| r["outcome"].as_str().unwrap())
        .collect();
    assert!(outcomes.contains(&"done"));
    assert!(outcomes.contains(&"failed"));
    assert!(outcomes.contains(&"refused"));
    assert!(rows.iter().all(|r| r["agent_id"] == agent_id));
    assert!(body.get("error").is_none());
}

/// 本番経路: セッションのツール実行が tool_logs 1 行になり、GET で読める。
#[tokio::test]
#[ignore = "old HTTP conversation route removed"]
async fn test_tool_logs_written_on_session_tool_call() {
    let (app, db, mock) = create_test_app_with_llm();
    let (agent_a, app) = create_test_agent_named(app, "Alice", "Curious").await;
    let (agent_b, app) = create_test_agent_named(app, "Bob", "Learner").await;
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "tool logs",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-learn-logs".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "tool_log_probe",
                "description": "probe",
                "situation_pattern": "when testing",
                "guidance": "record the row"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("learned");

    let before_memory = {
        let conn = db.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "Please learn from this."
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["responses"][0]["tool_calls_made"], 1);

    let after_memory = {
        let conn = db.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM memory_sessions", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    assert!(
        after_memory >= before_memory,
        "memory_sessions は減らさない: before={before_memory} after={after_memory}"
    );

    let responder = resp["responses"][0]["agent_id"].as_str().unwrap();
    let (status, body) = send_request(
        app,
        "GET",
        &format!("/api/agents/{responder}/tool-logs?limit=20"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().expect("array");
    assert_eq!(rows.len(), 1, "ツール 1 実行 = 1 行: {body}");
    assert_eq!(rows[0]["tool_name"], "learn_from_experience");
    assert_eq!(rows[0]["outcome"], "done");
    assert_eq!(rows[0]["session_id"], session_id);
    assert_eq!(rows[0]["agent_id"], responder);
}

