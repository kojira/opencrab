#[tokio::test]
async fn llm_tool_history_resolves_call_spawn_and_completion_by_ids() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    let session_id = "discord-history-test";
    let call = serde_json::json!({
        "id": "call-967",
        "type": "function",
        "function": {"name": "execute_shell", "arguments": "{\"command\":\"sleep 1\"}"}
    });
    let source_log_id;
    {
        let conn = db.lock().unwrap();
        source_log_id = opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.clone(),
                session_id: session_id.into(),
                log_type: "tool_call".into(),
                content: String::new(),
                speaker_id: Some(agent_id.clone()),
                turn_number: None,
                metadata_json: Some(serde_json::json!({
                    "tool_calls_json": serde_json::json!([call.clone()]).to_string()
                }).to_string()),
                created_at: None,
            },
        ).unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.clone(),
                session_id: session_id.into(),
                log_type: "tool_result".into(),
                content: r#"{"success":true,"data":{"status":"spawned","subtask_id":"sub-967"}}"#.into(),
                speaker_id: Some(agent_id.clone()),
                turn_number: None,
                metadata_json: Some(r#"{"tool_call_id":"call-967","tool_name":"execute_shell"}"#.into()),
                created_at: None,
            },
        ).unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.clone(),
                session_id: session_id.into(),
                log_type: "system".into(),
                content: r#"{"type":"subtask_completed","subtask_id":"sub-967","result":"done"}"#.into(),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        ).unwrap();
        opencrab_db::queries::insert_llm_log(&conn, &opencrab_db::queries::LlmLogRow {
            id: "llm-967".into(),
            agent_id: agent_id.clone(),
            session_id: Some(session_id.into()),
            model: Some("chatgpt:gpt-5.6-sol".into()),
            prompt: format!("execute_shell(→log:{source_log_id})"),
            response: "{}".into(),
            tool_calls: None,
            latency_ms: Some(1),
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            error_code: None,
            error_body: None,
            requested_at: None,
            trigger_message_id: None,
            is_bot_iteration: true,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            provider_tool_history: r#"{"state":"not_used","provider":"chatgpt","calls":[],"citations":[]}"#.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }).unwrap();
    }

    let (status, body) = send_request(
        app,
        "GET",
        &format!("/api/agents/{agent_id}/llm-logs/llm-967/tool-history"),
        None,
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entries"][0]["call"]["id"], "call-967");
    assert_eq!(body["entries"][0]["source_memory_log_id"], source_log_id);
    assert_eq!(body["entries"][0]["subtask_id"], "sub-967");
    assert_eq!(body["entries"][0]["completion"]["result"], "done");
    assert_eq!(body["provider_tool_history"]["state"], "not_used");
}
