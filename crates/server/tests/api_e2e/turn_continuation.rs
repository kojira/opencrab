// ==================== (#898) CONTINUE 途中イテレーションの REST 配送・保存 ====================
//
// DESIGN-TURN-CONTINUATION.md §12.2「CONTINUE で継続する各イテレーションの content（剥がし後）は、
// 最終応答と同じ既存経路（REST: responses への追加／…／memory_sessions: speech 保存）で配送・保存
// される。『次イテレーションの assistant メッセージに積む』だけでは未実装」。
// §13 #2「本文＋最終行 CONTINUE → 配送 本文1・保存 1・次 進む・残留なし」。
// §13 ターン合計 plain3「LLM 3・配送 3・保存 3」。§13.1 d「REST で各イテレーションは responses 配列に
// 1 要素ずつ追加（順序保持）」。
//
// 観測境界: REST responses 件数/本文/順序・memory_sessions speech 件数/本文・LLM 呼び出し回数・残留 CONTINUE。

/// §1 受け入れ（REST plain3）: 指示文「返信ツールを使わず 3 回に分けて投稿して」を観測境界で。
#[tokio::test]
async fn test_rest_continue_intermediate_speech_responses_and_saved() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Continuer", "TestPersona").await;

    // reply を使わない純テキスト 3 分割（plain3）。末尾 CONTINUE で継続、3 回目は継続せず終了。
    mock.push_text_response("REST-1回目。まず一つ⚡\nCONTINUE");
    mock.push_text_response("REST-2回目。次いこう⚡\nCONTINUE");
    mock.push_text_response("REST-3回目。これで最後⚡");

    let (status, resp) = send_request(
        app.clone(),
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

    // (§13.1 d) responses に 1 要素ずつ順序保持で 3 件。
    let bodies: Vec<String> = resp["responses"]
        .as_array()
        .expect("responses array")
        .iter()
        .map(|r| r["content"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        bodies.len(),
        3,
        "REST responses が 3 件でない（途中発話が responses に載っていない）: {bodies:?}"
    );
    assert!(
        bodies[0].contains("1回目"),
        "responses[0] が 1回目でない（順序保持）: {bodies:?}"
    );
    assert!(
        bodies[1].contains("2回目"),
        "responses[1] が 2回目でない（順序保持）: {bodies:?}"
    );
    assert!(
        bodies[2].contains("3回目"),
        "responses[2] が 3回目でない（順序保持）: {bodies:?}"
    );
    // 残留 CONTINUE なし（配送本文）。
    assert!(
        bodies.iter().all(|b| !b.contains("CONTINUE")),
        "responses に CONTINUE 残留: {bodies:?}"
    );

    // memory_sessions に agent speech 3 件・CONTINUE 非残留。
    let speeches: Vec<String> = session_logs(&db, &session_id)
        .into_iter()
        .filter(|l| l.log_type == "speech" && l.content.contains("REST-"))
        .map(|l| l.content)
        .collect();
    assert_eq!(
        speeches.len(),
        3,
        "memory_sessions の agent speech が 3 件でない（途中発話が保存されていない）: {speeches:?}"
    );
    assert!(
        speeches.iter().all(|s| !s.contains("CONTINUE")),
        "speech に CONTINUE 残留: {speeches:?}"
    );

    // LLM は 3 回（末尾 CONTINUE が 2 回の追加イテレーションを起こす）。
    assert_eq!(mock.system_prompts().len(), 3, "LLM 呼び出しが 3 回でない");
}

/// §13 #3「CONTINUE のみ（本文空）→ 配送 0・保存 0・次へ進む」を REST 観測境界で。
#[tokio::test]
async fn test_rest_continue_only_iteration_adds_no_response_but_progresses() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Continuer3", "TestPersona").await;

    mock.push_text_response("RC3-A本文⚡\nCONTINUE");
    mock.push_text_response("CONTINUE"); // 本文空・継続のみ（#3）
    mock.push_text_response("RC3-C本文⚡");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "続けて", "user_id": "u1"})),
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
    // CONTINUE のみのイテレーションは responses に載らない（本文 A と C の 2 件・順序保持）。
    assert_eq!(
        bodies.len(),
        2,
        "CONTINUE のみが responses に載った/本文が落ちた: {bodies:?}"
    );
    assert!(
        bodies[0].contains("RC3-A") && bodies[1].contains("RC3-C"),
        "順序/本文が違う: {bodies:?}"
    );

    let speeches: Vec<String> = session_logs(&db, &session_id)
        .into_iter()
        .filter(|l| l.log_type == "speech" && l.content.contains("RC3-"))
        .map(|l| l.content)
        .collect();
    assert_eq!(
        speeches.len(),
        2,
        "CONTINUE のみを保存に含めた/本文を落とした: {speeches:?}"
    );

    // LLM は 3 回（空継続も 1 イテレーション進む）。
    assert_eq!(mock.system_prompts().len(), 3, "LLM は 3 回のはず");
}

/// §13 #16「CONTINUE 連鎖が max_iterations（depth0=30）到達 → 各イテレーション分すべて配送・
/// 保存・上限で停止・stopped_by_limit」を REST 観測境界で。30 連続 CONTINUE で上限に達し、
/// 各イテレーションの本文が responses/speech にイテレーション数ぶん載る（現状は最終のみ）。
#[tokio::test]
async fn test_rest_continue_hits_max_iterations_delivers_each_iteration() {
    let (app, db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Continuer16", "TestPersona").await;

    // 30 連続 CONTINUE（本文つき）。終端しないので depth0 の max_iterations=30 に達する。
    for i in 1..=30 {
        mock.push_text_response(&format!("I16-{i}本文⚡\nCONTINUE"));
    }

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "ずっと続けて", "user_id": "u1"})),
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

    // 各イテレーションの本文が responses に載る（イテレーション数ぶん = 30）。現状は最終のみ。
    let per_iteration = bodies.iter().filter(|b| b.contains("I16-")).count();
    assert_eq!(
        per_iteration, 30,
        "各イテレーションの本文が responses に載っていない（配送がイテレーション数ぶんでない）: {per_iteration} 件"
    );

    // memory_sessions にも各イテレーション分保存される（30 件）。
    let saved = session_logs(&db, &session_id)
        .into_iter()
        .filter(|l| l.log_type == "speech" && l.content.contains("I16-"))
        .count();
    assert_eq!(
        saved, 30,
        "各イテレーションの本文が memory_sessions に保存されていない: {saved} 件"
    );

    // LLM は上限ぶん（30 回）呼ばれる。
    assert_eq!(
        mock.system_prompts().len(),
        30,
        "LLM が上限（30）まで呼ばれていない"
    );

    // stopped_by_limit の観測: 上限到達の打ち切り応答が最終として配送される。
    assert!(
        bodies.iter().any(|b| b.contains("maximum number of steps")),
        "上限停止（stopped_by_limit）の打ち切り応答が配送されていない: {bodies:?}"
    );
}

