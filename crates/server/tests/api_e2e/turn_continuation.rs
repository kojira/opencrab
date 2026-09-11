// 明示終端まで既定継続する REST 配送・保存契約。

#[tokio::test]
async fn test_rest_continue_intermediate_speech_responses_and_saved() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Continuer", "TestPersona").await;

    mock.push_text_response("REST-1回目。まず一つ⚡");
    mock.push_text_response("REST-2回目。次いこう⚡");
    mock.push_text_response("REST-3回目。これで最後⚡\nNO_REPLY");

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({
            "content": "返信ツールを使わず 3 回に分けて投稿して",
            "user_id": "u1"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = format!("agent-msg-{agent_id}-u1");

    let bodies: Vec<String> = resp["responses"]
        .as_array()
        .expect("responses array")
        .iter()
        .map(|r| r["content"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(bodies.len(), 3, "途中発話が順に返らない: {bodies:?}");
    assert!(bodies[0].contains("1回目"));
    assert!(bodies[1].contains("2回目"));
    assert!(bodies[2].contains("3回目"));
    assert!(bodies.iter().all(|body| !body.contains("NO_REPLY")));

    let speeches: Vec<String> = session_logs(&db, &session_id)
        .into_iter()
        .filter(|log| log.log_type == "speech" && log.content.contains("REST-"))
        .map(|log| log.content)
        .collect();
    assert_eq!(speeches.len(), 3);
    assert!(speeches.iter().all(|speech| !speech.contains("NO_REPLY")));
    assert_eq!(mock.system_prompts().len(), 3);
}


/// iteration 上限では新しい発言を作らず、途中発話を残して資源切れイベントを保存する。
#[tokio::test]
async fn test_rest_continue_hits_max_iterations_delivers_each_iteration() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Continuer16", "TestPersona").await;

    for i in 1..=30 {
        mock.push_text_response(&format!("I16-{i}本文⚡"));
    }

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "ずっと続けて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = format!("agent-msg-{agent_id}-u1");
    let bodies: Vec<String> = resp["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["content"].as_str().unwrap_or("").to_string())
        .collect();

    assert_eq!(bodies.iter().filter(|body| body.contains("I16-")).count(), 30);
    assert!(
        bodies
            .iter()
            .all(|body| !body.contains("maximum number of steps")),
        "資源切れメッセージを投稿してはならない: {bodies:?}"
    );
    let logs = session_logs(&db, &session_id);
    assert_eq!(
        logs.iter()
            .filter(|log| log.log_type == "speech" && log.content.contains("I16-"))
            .count(),
        30
    );
    let exhausted: Vec<_> = logs
        .iter()
        .filter(|log| log.log_type == "system" && log.content.contains("turn_exhausted"))
        .collect();
    assert_eq!(exhausted.len(), 1, "資源切れイベントが1件でない");
    assert!(exhausted[0].content.contains("iteration_limit"));
    assert_eq!(mock.system_prompts().len(), 30);
}
