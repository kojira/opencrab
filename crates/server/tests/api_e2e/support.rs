/// Create test app using the REAL server router (same as production).
fn create_test_app() -> Router {
    create_test_app_with_db().0
}

/// `create_test_app` と同じアプリを作り、同じ DB ハンドルも返す。
///
/// API を経由せずに行を書き込みたいテスト（入口の正規化が入る前に保存された
/// レガシー行の再現）で使う。
fn create_test_app_with_db() -> (Router, opencrab_db::Db) {
    let (state, db) = create_test_state(0.5);
    (create_router(state), db)
}

/// 既定ヘルパと同じ `AppState` を組むが、`compaction_ratio` だけ引数で差し替える。
/// compaction_ratio を state から読んでいる経路（例: model_pricing 一覧 API）を
/// 恒真にならない値で検証するために使う。
fn create_test_state(compaction_ratio: f64) -> (AppState, opencrab_db::Db) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let mut cfg = opencrab_server::test_config("mock", "test");
    // 既定を明示的に置かない（default_provider = ""）。テストの base は provider 未定義で、
    // provider を DB オーバーライドで足す reload 経路（build_llm_router）が、コード既定の
    // sentinel "openai" と食い違って default_provider 検証で弾かれるのを避ける（#660）。
    // 空 default_provider は「既定なし＝未設定」として検証をスキップする正規の状態。
    // そのとき default_model は provider 付きの完全な spec（既定モデルは "mock:test"）。
    cfg.llm.default_provider = String::new();
    cfg.llm.default_model = "mock:test".to_string();
    cfg.llm.compaction_ratio = compaction_ratio;
    let state = AppState::new(db.clone(), LlmRouter::new(), cfg);
    (state, db)
}

// ==================== Helper ====================

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
        "PATCH" => builder.method("PATCH"),
        "DELETE" => builder.method("DELETE"),
        _ => panic!("unsupported method"),
    };

    if method == "POST" || method == "PUT" || method == "PATCH" {
        builder = builder.header("content-type", "application/json");
    }

    let req = builder.body(body).unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!(body_bytes.to_vec()));
    (status, json)
}

/// Create an agent via API and return its ID.
async fn create_test_agent(app: Router) -> (String, Router) {
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": "Test Agent",
            "persona_name": "TestPersona"
        })),
    )
    .await;
    let agent_id = resp["id"].as_str().unwrap().to_string();
    (agent_id, app)
}

/// D-1080: a direct `run_agent_response` on a real session needs its `sessions` row.
fn insert_member_session(state: &AppState, session_id: &str, agent_id: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: session_id.to_string(),
            mode: "autonomous".to_string(),
            theme: session_id.to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: serde_json::json!([agent_id]).to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}
