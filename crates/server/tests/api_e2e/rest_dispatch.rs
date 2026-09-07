// ==================== #169: REST の非ブロック dispatch ====================

/// REST セッションのログを新しい順に取る小さなヘルパ。
fn session_logs(
    db: &opencrab_db::Db,
    session_id: &str,
) -> Vec<opencrab_db::queries::SessionLogRow> {
    let conn = db.lock().unwrap();
    opencrab_db::queries::list_recent_session_logs(&conn, session_id, 100).unwrap()
}

fn session_status(db: &opencrab_db::Db, session_id: &str) -> Option<String> {
    let conn = db.lock().unwrap();
    opencrab_db::queries::get_session(&conn, session_id)
        .ok()
        .flatten()
        .map(|s| s.status)
}

/// 即完了しない fake subtask を registry へ登録する（走行中 subtask の代役）。
fn insert_running_subtask(
    registry: &opencrab_actions::SubtaskRegistry,
    subtask_id: &str,
    parent_session_id: &str,
    agent_id: &str,
) -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(std::future::pending::<()>());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: handle.abort_handle(),
            session_id: format!("subtask-{subtask_id}"),
            parent_session_id: parent_session_id.to_string(),
            agent_id: agent_id.to_string(),
            label: "long job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Agent,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    handle
}

/// #169: `POST /api/agents/{id}/messages` はツールを background subtask として dispatch する
/// （メインを塞がない）。ツール結果は inline の `{"success":...}` ではなく
/// `{"status":"spawned"}` になり、完了本文は親セッションログへ着地する（取得口 = セッションログ）。
#[tokio::test]
async fn test_rest_message_dispatches_tool_as_background_subtask() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Dispatcher", "TestPersona").await;

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-dispatch-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "learn_from_experience".to_string(),
            arguments: serde_json::json!({
                "skill_name": "background_work",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            })
            .to_string(),
        },
    }]);
    mock.push_text_response("バックグラウンドで実行を開始しました");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "スキルを覚えて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let session_id = resp["session_id"].as_str().unwrap().to_string();
    assert_eq!(session_id, format!("agent-msg-{agent_id}-u1"));

    // dispatch された（tool_result が spawned）。inline 実行なら status フィールドは無い。
    let logs = session_logs(&db, &session_id);
    assert!(
        logs.iter()
            .any(|l| l.log_type == "tool_result" && l.content.contains("\"status\":\"spawned\"")),
        "REST でツールが background subtask へ dispatch されていない: {:?}",
        logs.iter()
            .map(|l| (l.log_type.clone(), l.content.clone()))
            .collect::<Vec<_>>()
    );

    // 完了本文（subtask_completed）が親セッションログへ着地する = REST の取得口。
    let mut settled = false;
    for _ in 0..100 {
        if session_logs(&db, &session_id)
            .iter()
            .any(|l| l.content.contains("subtask_completed"))
        {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        settled,
        "dispatch した subtask の完了本文が親セッションログへ永続化されない"
    );

    // 決着後は registry が空（settle_completed が除去する）。
    assert!(!state.subtask_registries.has_running(&session_id));
}

/// #632: 存在しない `agent_id` への REST `POST /api/agents/{id}/messages` も同じ穴を
/// 持っていた。サーバ側チョークポイント（`process::run_agent_response`）で弾き、404 に揃える。
#[tokio::test]
async fn test_rest_messages_unknown_agent_is_rejected_without_running() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let bogus = "does-not-exist-632-rest";

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{bogus}/messages"),
        Some(serde_json::json!({ "content": "hi", "user_id": "u1" })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(resp["error"], format!("agent not found: {bogus}"));
    assert!(
        mock.system_prompts().is_empty(),
        "存在しないエージェントで LLM ターンが走った"
    );
}

/// #632 回帰: 存在するエージェントは REST `POST /messages` で従来どおり 200 で走る。
#[tokio::test]
async fn test_rest_messages_existing_agent_runs() {
    let (app, _db, mock, _state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "RestReal", "TestPersona").await;
    mock.push_text_response("やあ");

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({ "content": "hi", "user_id": "u1" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "存在するエージェントは 200 で走る");
    assert_eq!(resp["session_id"], format!("agent-msg-{agent_id}-u1"));
    assert!(resp["responses"].is_array(), "応答配列が返る: {resp}");
}

// ==================== #640: REST ターンの直列化 ====================

/// `chat_completion` の中に同時に何本の run が居たかを観測する LLM プロバイダ。
///
/// `hold` の間 sleep して重なりの窓を作る。REST の `run_agent_response` が共有ロック
/// （`state.session_locks.run_serialized`）を通っていれば、同一セッションの run は重ならず
/// `max_in_flight` は 1 を超えない。別セッションは重なり 2 以上になりうる。web の
/// `same_session_serializes` / `different_sessions_run_concurrently` と同型の観測。
struct SerializationProbe {
    in_flight: std::sync::atomic::AtomicUsize,
    max_in_flight: std::sync::atomic::AtomicUsize,
    hold: std::time::Duration,
}

impl SerializationProbe {
    fn new(hold: std::time::Duration) -> Self {
        Self {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            max_in_flight: std::sync::atomic::AtomicUsize::new(0),
            hold,
        }
    }

    fn max(&self) -> usize {
        self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for SerializationProbe {
    fn name(&self) -> &str {
        "mock"
    }

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.hold).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("ok"),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage::default(),
            created: 0,
        })
    }
}

/// `SerializationProbe` を既定プロバイダに仕込んだ app を作る。既定モデル `mock:test` が
/// probe（`name()=="mock"`）へ解決する。
fn create_probe_app(
    hold: std::time::Duration,
) -> (Router, opencrab_db::Db, Arc<SerializationProbe>) {
    let (mut state, db) = create_test_state(0.5);
    let probe = Arc::new(SerializationProbe::new(hold));
    let mut router = LlmRouter::new();
    router.add_provider(probe.clone() as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");
    state.llm_router = opencrab_server::SharedLlmRouter::new(router);
    // #703: 既定モデル `mock:test` を `model_pricing` に登録する。#676 で「出力上限が未登録の
    // モデルは fail loud」になったため、登録が無いと run が LLM へ届く前に止まり、probe が
    // 1 度も呼ばれない（HTTP は 200 のままで、本文にエラー文字列が入るだけなので気づけない）。
    // このヘルパを使うテストは**直列化**を見るものなので、モデル解決で落ちてはいけない。
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "mock".to_string(),
                model: "test".to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(4_096),
            },
        )
        .expect("probe 用モデルの登録");
    }
    (opencrab_server::create_router(state), db, probe)
}

/// #640: 同一セッション（同じ agent_id + user_id）への並行 2 POST は直列に走る
/// （`run_agent_response` の中で同時に走っている run が 1 を超えない）。
#[tokio::test]
async fn test_rest_agent_messages_serialize_same_session() {
    let (app, _db, probe) = create_probe_app(std::time::Duration::from_millis(150));
    let (agent_id, app) = create_test_agent_named(app, "SerialRest", "TestPersona").await;

    let a1 = app.clone();
    let id1 = agent_id.clone();
    let h1 = tokio::spawn(async move {
        send_request(
            a1,
            "POST",
            &format!("/api/agents/{id1}/messages"),
            Some(serde_json::json!({"content": "m1", "user_id": "u1"})),
        )
        .await
    });
    let a2 = app.clone();
    let id2 = agent_id.clone();
    let h2 = tokio::spawn(async move {
        send_request(
            a2,
            "POST",
            &format!("/api/agents/{id2}/messages"),
            Some(serde_json::json!({"content": "m2", "user_id": "u1"})),
        )
        .await
    });

    let (s1, _) = h1.await.unwrap();
    let (s2, _) = h2.await.unwrap();
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        probe.max(),
        1,
        "同一セッションへの並行 POST の run が重なった（直列化が効いていない）"
    );
}

/// #640: 別セッション（別 user_id）への並行 POST は従来どおり並行に走る
/// （ロックの粒度は session_id 単位。無関係なセッションを詰まらせない）。
#[tokio::test]
async fn test_rest_agent_messages_run_concurrently_across_sessions() {
    let (app, _db, probe) = create_probe_app(std::time::Duration::from_millis(300));
    let (agent_id, app) = create_test_agent_named(app, "ConcurrentRest", "TestPersona").await;

    let a1 = app.clone();
    let id1 = agent_id.clone();
    let h1 = tokio::spawn(async move {
        send_request(
            a1,
            "POST",
            &format!("/api/agents/{id1}/messages"),
            Some(serde_json::json!({"content": "m1", "user_id": "userA"})),
        )
        .await
    });
    let a2 = app.clone();
    let id2 = agent_id.clone();
    let h2 = tokio::spawn(async move {
        send_request(
            a2,
            "POST",
            &format!("/api/agents/{id2}/messages"),
            Some(serde_json::json!({"content": "m2", "user_id": "userB"})),
        )
        .await
    });

    let (s1, _) = h1.await.unwrap();
    let (s2, _) = h2.await.unwrap();
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        probe.max(),
        2,
        "別セッションへの並行 POST が直列化された（粒度が session ではなく global になっている）"
    );
}

/// #169: registry が `AppState` 経由で共有されるため、REST から `cancel_subtask` が
/// 走行中 subtask に到達できる（使い捨て registry では常に not found だった）。
#[tokio::test]
async fn test_rest_cancel_subtask_reaches_shared_registry() {
    let (app, db, mock, state) = create_test_app_with_state();
    let (agent_id, app) = create_test_agent_named(app, "Canceller", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");

    // ハンドラが使うのと同一の registry（AppState 保持）へ走行中 subtask を登録する。
    let registry = state.subtask_registries.registry_for(&session_id);
    let handle = insert_running_subtask(&registry, "st-rest-1", &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-cancel-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "cancel_subtask".to_string(),
            arguments: serde_json::json!({"subtask_id": "st-rest-1"}).to_string(),
        },
    }]);
    mock.push_text_response("止めました");

    let (status, _resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({"content": "さっきのを止めて", "user_id": "u1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 停止が到達した: registry から除去され、タスクが abort されている。
    assert!(
        !registry.contains_key("st-rest-1"),
        "cancel_subtask が共有 registry に到達していない（not found）"
    );
    assert!(handle.await.unwrap_err().is_cancelled());

    let logs = session_logs(&db, &session_id);
    assert!(
        logs.iter().any(|l| l.log_type == "tool_cancelled"),
        "親セッションログに tool_cancelled が記録されない"
    );
    assert!(
        !logs
            .iter()
            .any(|l| l.log_type == "tool_result" && l.content.contains("not found")),
        "cancel_subtask が not found を返している: {:?}",
        logs.iter()
            .filter(|l| l.log_type == "tool_result")
            .map(|l| l.content.clone())
            .collect::<Vec<_>>()
    );
}

