/// #291: 対話ターンでは evaluator を呼ばない。
///
/// 撤去前は「契約付き active タスクがあり、その run がツールを使った」場合に、
/// 生成 run の直後で毎回 evaluator を 1 回まわし、採点結果を `session_logs`
/// (log_type=evaluation) と台帳 progress に書いていた。会話再構築でその行が
/// 「次ターンでギャップを埋めろ」という指示文つきで復元され、直前のユーザー
/// 発言より採点の圧が勝つ事故（#291）につながった。
///
/// このテストは旧実装の**発火条件をすべて満たした**うえで、
/// - LLM 呼び出しが生成 run のぶん（2 回）だけであること
/// - `evaluation` ログも `[evaluation]` 進捗も残らないこと
/// を確かめる。旧実装ならモックのキューが尽き（3 回目を要求し）、記録も残る。
#[tokio::test]
#[ignore = "old HTTP conversation route removed"]
async fn test_dialogue_turn_does_not_invoke_evaluator() {
    let (app, db, mock) = create_test_app_with_llm();

    let (agent_a, app) = create_test_agent_named(app, "Asker", "Curious Mind").await;
    let (agent_b, app) = create_test_agent_named(app, "Worker", "Diligent Hand").await;

    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/sessions",
        Some(serde_json::json!({
            "theme": "契約付きタスクの遂行",
            "participant_ids": [&agent_a, &agent_b]
        })),
    )
    .await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // 旧 verify 段の発火条件その 1: 応答側に contract 非空の active タスクがある。
    let task_id = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::insert_task_ledger(
            &conn,
            &agent_b,
            &session_id,
            "スキルを 1 つ作る",
            Some("learn_from_experience で新しいスキルが DB に入っていること"),
        )
        .unwrap()
    };

    // 発火条件その 2: この run が実際にツールを実行する。
    // キューは生成 run のぶん（tool_call → text）だけ積む。
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-291".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "contract_work",
                "description": "契約付きタスクを進める",
                "situation_pattern": "when a contract task is active",
                "guidance": "証拠を残しながら進める"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("スキルを作りました。");

    let (status, resp) = send_request(
        app,
        "POST",
        &format!("/api/sessions/{session_id}/messages"),
        Some(serde_json::json!({
            "agent_id": agent_a,
            "content": "契約どおり進めてほしい"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let responses = resp["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0]["tool_calls_made"].as_i64().unwrap(),
        1,
        "テストの前提: 旧 verify 段の発火条件（ツールを使った run）を満たすこと"
    );

    // 生成 run の 2 回だけ。3 回目があれば evaluator がまだ対話ターンにいる。
    let calls = mock.system_prompts();
    assert_eq!(
        calls.len(),
        2,
        "対話ターンで余計な LLM 呼び出しが走っている（evaluator の残留）: {calls:#?}"
    );

    let conn = db.lock().unwrap();
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, &session_id).unwrap();
    assert!(
        logs.iter().all(|l| l.log_type != "evaluation"),
        "対話ターンが evaluation ログを書いている: {:#?}",
        logs.iter().map(|l| &l.log_type).collect::<Vec<_>>()
    );

    let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 50).unwrap();
    assert!(
        progress.iter().all(|p| !p.content.contains("[evaluation]")),
        "対話ターンが台帳へ採点を書いている: {progress:#?}"
    );
}

