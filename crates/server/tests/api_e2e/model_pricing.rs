// ==================== #412: context_window 未登録モデルは設定時に弾く ====================

/// `model_pricing` に行を入れる唯一の経路。ここが無かったので誰も入れられず、
/// 空でも既定値で黙って動いていた。
#[tokio::test]
async fn test_model_pricing_put_then_list() {
    let app = create_test_app();

    let (status, resp) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": "testprov",
            "model": "testmodel",
            "input_price_per_1m": 1.5,
            "output_price_per_1m": 3.0,
            "context_window": 200000
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["saved"], true);

    let (status, resp) = send_request(app, "GET", "/api/llm/model-pricing", None).await;
    assert_eq!(status, StatusCode::OK);
    let models = resp["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["provider"], "testprov");
    assert_eq!(models[0]["context_window"], 200000);
}

/// `context_window` こそが登録の目的なので、0 以下は受け付けない。
/// （通してしまうと「登録済みなのに予算が決まらない」行が作れる）
#[tokio::test]
async fn test_model_pricing_rejects_non_positive_context_window() {
    let app = create_test_app();
    let (status, _) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": "testprov", "model": "testmodel", "context_window": 0
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 弾いた以上、行は作られていない。
    let (_, resp) = send_request(app, "GET", "/api/llm/model-pricing", None).await;
    assert!(resp["models"].as_array().unwrap().is_empty());
}

async fn register_model(app: Router, provider: &str, model: &str, window: i64) {
    // #676: テストの router は空でプロバイダ能力が既定（送る＝登録必須）に倒れるため、
    // 「完全登録」を表すには max_output_tokens も入れる（context_window だけではモデル変更
    // ゲートを通らない）。ゲートの案Y 条件分岐は core の単体テストで担保する。
    let (status, _) = send_request(
        app,
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": provider, "model": model,
            "context_window": window, "max_output_tokens": 8192
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn agent_model(app: Router, agent_id: &str) -> serde_json::Value {
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    resp["model"].clone()
}

#[tokio::test]
async fn test_patch_agent_rejects_unregistered_model() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:unregistered"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["updated"], false);
    let err = resp["error"].as_str().unwrap();
    assert!(err.contains("model_pricing"), "{err}");
    assert!(err.contains("/api/llm/model-pricing"), "{err}");

    // 拒否した設定は保存されていない。
    assert_eq!(agent_model(app, &agent_id).await, serde_json::Value::Null);
}

#[tokio::test]
async fn test_patch_agent_accepts_registered_model() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    register_model(app.clone(), "testprov", "testmodel", 200_000).await;

    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:testmodel"})),
    )
    .await;
    assert_eq!(resp["updated"], true);
    assert_eq!(agent_model(app, &agent_id).await, "testprov:testmodel");
}

/// グローバル既定へ戻す操作は検証の対象外。既定側は config のホットリロードで
/// 検証するので、ここで塞ぐと戻せなくなる。
///
/// クリアの表現は**空文字**。serde の `Option<Option<_>>` は JSON null を
/// 「変更なし」に潰すため（`apply_agent_patch` の reasoning_effort に同趣旨のコメント）。
#[tokio::test]
async fn test_patch_agent_can_clear_model_without_registration() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    register_model(app.clone(), "testprov", "testmodel", 200_000).await;
    send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:testmodel"})),
    )
    .await;

    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": ""})),
    )
    .await;
    assert_eq!(resp["updated"], true);
    assert_eq!(agent_model(app, &agent_id).await, "");
}

#[tokio::test]
async fn test_put_agent_rejects_unregistered_model() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (_, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Test Agent",
            "persona_name": "TestPersona",
            "model": "testprov:unregistered"
        })),
    )
    .await;
    assert_eq!(resp["updated"], false);
    assert!(resp["error"].as_str().unwrap().contains("model_pricing"));
}

/// **既存の設定を壊さない。** 登録が始まる前から `agents.model` に入っていた値は、
/// そのまま送り直す限り弾かれない（識別情報だけを編集する PUT が通ること）。
/// 検証が効くのは**新しく設定するとき**だけ。
#[tokio::test]
async fn test_put_agent_keeps_existing_unregistered_model() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;

    // 検証が入る前の状態を再現: 未登録モデルを API を経由せず直接書き込む。
    {
        let conn = db.lock().unwrap();
        let mut row = opencrab_db::queries::get_agent(&conn, &agent_id)
            .unwrap()
            .unwrap();
        row.model = Some("testprov:legacy".to_string());
        opencrab_db::queries::upsert_agent(&conn, &row).unwrap();
    }

    let (_, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Renamed",
            "persona_name": "TestPersona",
            "model": "testprov:legacy"
        })),
    )
    .await;
    assert_eq!(resp["updated"], true, "{resp}");
    assert_eq!(agent_model(app, &agent_id).await, "testprov:legacy");
}

/// 上と同じ状況から**別の未登録モデルへ移す**のは弾く。
/// 「既存値は素通し」が「未登録なら何でも通る」に化けていないこと。
#[tokio::test]
async fn test_put_agent_rejects_switching_to_another_unregistered_model() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    {
        let conn = db.lock().unwrap();
        let mut row = opencrab_db::queries::get_agent(&conn, &agent_id)
            .unwrap()
            .unwrap();
        row.model = Some("testprov:legacy".to_string());
        opencrab_db::queries::upsert_agent(&conn, &row).unwrap();
    }

    let (_, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({
            "name": "Renamed",
            "persona_name": "TestPersona",
            "model": "testprov:another"
        })),
    )
    .await;
    assert_eq!(resp["updated"], false);
    assert_eq!(agent_model(app, &agent_id).await, "testprov:legacy");
}

/// 投入 API は provider/model を trim して保存し、gate も同じ正規化で引く。
/// 揃っていないと「登録したのに未登録と言われる」になる。
#[tokio::test]
async fn test_model_pricing_trim_is_consistent_between_put_and_gate() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    let (status, _) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/model-pricing",
        Some(serde_json::json!({
            "provider": "  testprov  ", "model": "  testmodel  ",
            "context_window": 200000, "max_output_tokens": 8192
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 空白なしの spec で通る（保存側が trim されている）。testprov は空 router で「送る」
    // 既定に倒れるため、モデル変更ゲートは max_output_tokens も要求する（#676 案Y）。上で登録済み。
    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": "testprov:testmodel"})),
    )
    .await;
    assert_eq!(resp["updated"], true, "{resp}");

    // 空白入りの spec でも通る（参照側も trim されている）。
    let (_, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({"model": " testprov : testmodel "})),
    )
    .await;
    assert_eq!(resp["updated"], true, "{resp}");
}

