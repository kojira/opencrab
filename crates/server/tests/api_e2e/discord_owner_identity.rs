// ==================== Discord owner normalization ====================

/// per-agent Discord 設定を保存する（owner を明示指定）。
async fn set_agent_owner(app: Router, agent_id: &str, owner_discord_id: &str) -> Router {
    let (status, resp) = send_request(
        app.clone(),
        "PUT",
        &format!("/api/agents/{agent_id}/discord"),
        Some(serde_json::json!({
            "bot_token": "test-token",
            "owner_discord_id": owner_discord_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["ok"], true, "discord config must be saved: {resp}");
    app
}

/// `POST /api/agents/{id}/messages` を叩いて `caller_type` を返す。
async fn caller_type_for(app: Router, agent_id: &str, user_id: &str) -> (Router, String) {
    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({
            "content": "hello",
            "user_id": user_id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let caller_type = resp["caller_type"]
        .as_str()
        .unwrap_or_else(|| panic!("response must carry caller_type: {resp}"))
        .to_string();
    (app, caller_type)
}

/// owner 未設定（per-agent Discord 設定の owner が空文字）のとき、空の `user_id`
/// で呼んでも Owner 権限にならない。
#[tokio::test]
async fn test_empty_user_id_is_not_owner_when_owner_unset() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, "").await;
    assert_ne!(
        caller_type, "owner",
        "empty user_id must not be promoted to owner when owner is unset"
    );
    assert_eq!(caller_type, "agent");
}

/// 空白のみの owner を保存しても owner は「未設定」のままで、空白のみの `user_id`
/// で呼んでも Owner 権限にならない。
///
/// PUT の入口 trim により `" "` は `""` として保存されるため、これは「空 owner」の
/// 検証になる（空白のまま保存された**レガシー行**の経路は
/// `test_legacy_whitespace_only_owner_row_matches_nobody` が受け持つ）。
#[tokio::test]
async fn test_whitespace_user_id_is_not_owner_when_owner_blank() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, " ").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, " ").await;
    assert_ne!(
        caller_type, "owner",
        "whitespace-only owner must be treated as unset"
    );
    assert_eq!(caller_type, "agent");
}

/// [#848 回帰] REST 経路（`POST /api/agents/{id}/messages`）はボディの `user_id` を
/// **自称値**として扱い、それが設定済み owner の識別子と一致しても owner へ昇格させない。
///
/// 以前はここが `caller_type == "owner"` を期待していた（＝owner 識別子を知る到達者が
/// ボディに書くだけで owner 専用アクション `execute_shell` 等へ届く信頼境界の穴・#848）。
/// 案A（owner 判定は認証済み識別子のみ）に合わせ、REST では owner にならないことを固定する。
/// gateway 車線（Nostr / Discord）の owner 判定は認証済み識別子を刻むため不変。
#[tokio::test]
async fn test_rest_body_user_id_matching_owner_is_not_promoted() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, "123456789012345678").await;
    assert_ne!(
        caller_type, "owner",
        "REST の自称 user_id が owner に昇格した（#848）"
    );
    assert_eq!(caller_type, "agent");
}

/// 負のコントロール: owner 設定済みでも、**別の** `user_id` は Owner にならない
/// （ガードが過少に振れて誰でも owner になっていない）。
#[tokio::test]
async fn test_other_user_is_not_owner_when_owner_configured() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (_app, caller_type) = caller_type_for(app, &agent_id, "987654321098765432").await;
    assert_ne!(
        caller_type, "owner",
        "a different user_id must not be recognized as owner"
    );
    assert_eq!(caller_type, "agent");
}

/// [#848 回帰] 空白付きで保存されたレガシー owner 行があっても、REST 経路では自称
/// `user_id` を owner へ昇格させない。
///
/// `is_owner_id` の trim 比較そのもの（レガシー行が padded でも一致する契約）は
/// core の単体テストと共有 1 実装（`caller_identity` の web 車線テスト）が担保する。
/// ここは **REST 経路が認証済み識別子でない自称値を owner にしない**ことだけを見る。
#[tokio::test]
async fn test_rest_padded_owner_row_body_is_not_promoted() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    {
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::patch_agent_discord_config(
            &conn,
            &agent_id,
            None,
            Some("  123456789012345678\n"),
        )
        .unwrap());
    }

    let (app, caller_type) = caller_type_for(app, &agent_id, "123456789012345678").await;
    assert_ne!(
        caller_type, "owner",
        "REST では padded owner 行に一致する自称 user_id でも owner にしない（#848）"
    );
    assert_eq!(caller_type, "agent");
    // 別 ID も当然 owner ではない。
    let (_app, caller_type) = caller_type_for(app, &agent_id, "987654321098765432").await;
    assert_ne!(caller_type, "owner");
}

/// レガシー行の owner が空白のみなら「未設定」として扱い、誰も owner に昇格させない。
#[tokio::test]
async fn test_legacy_whitespace_only_owner_row_matches_nobody() {
    let (app, db) = create_test_app_with_db();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    {
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::patch_agent_discord_config(
            &conn,
            &agent_id,
            None,
            Some(" \t\n")
        )
        .unwrap());
    }

    let mut app = app;
    for user_id in [" \t\n", " ", "", "123456789012345678"] {
        let (next, caller_type) = caller_type_for(app, &agent_id, user_id).await;
        app = next;
        assert_ne!(
            caller_type, "owner",
            "whitespace-only legacy owner must match nobody (user_id={user_id:?})"
        );
    }
}

/// `user_id` の前後空白はハンドラ入口で 1 回だけ正規化され、セッションキー・
/// `speaker_id` で同じ値が使われる（同じ相手が空白差で別セッションに割れない）。
///
/// #848 以降 REST は自称 `user_id` を owner に昇格させないため `caller_type` は `agent`。
/// このテストの主眼は owner 判定ではなく **trim の一貫性**（session_id が trim 済み値を使う）。
#[tokio::test]
async fn test_user_id_is_trimmed_consistently() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (status, resp) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/messages"),
        Some(serde_json::json!({
            "content": "hello",
            "user_id": "  123456789012345678 ",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // REST の自称 user_id は owner にならない（#848）。trim 済みでも owner 昇格しない。
    assert_eq!(resp["caller_type"], "agent");
    assert_eq!(
        resp["session_id"],
        format!("agent-msg-{agent_id}-123456789012345678"),
        "session id must use the trimmed user_id: {resp}"
    );
}

/// `PUT /api/agents/{id}/discord` は owner を trim して保存する。
///
/// 前後空白付きのまま保存すると、trim 済み比較を行う経路（`is_owner_id`）では
/// owner と認識されるのに、生比較が残る下位経路（form/modal）だけ無言で拒否される
/// 半端な状態になる。入口で正規化して防ぐ（判定述語の共通化は #174）。
#[tokio::test]
async fn test_owner_discord_id_is_trimmed_on_save() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    let app = set_agent_owner(app, &agent_id, "  123456789012345678\n").await;

    // 保存値そのものが trim されている。
    let (status, resp) = send_request(
        app.clone(),
        "GET",
        &format!("/api/agents/{agent_id}/discord"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["owner_discord_id"], "123456789012345678");
}

/// `PATCH /api/agents/{id}/discord` も owner を trim して保存する。
///
/// PUT だけ直しても、ダッシュボードからの部分更新（PATCH）経路から空白付きの owner が
/// 入り込む余地が残る（PUT 版は `test_owner_discord_id_is_trimmed_on_save`）。
#[tokio::test]
async fn test_owner_discord_id_is_trimmed_on_patch() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;
    // PATCH は設定済みの行にしか効かないので、まず PUT で作る。
    let app = set_agent_owner(app, &agent_id, "123456789012345678").await;

    let (status, resp) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}/discord"),
        Some(serde_json::json!({
            "owner_discord_id": "  987654321098765432\n",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["ok"], true, "patch must succeed: {resp}");
    assert_eq!(
        resp["owner_discord_id"], "987654321098765432",
        "PATCH は owner を trim して保存する: {resp}"
    );

    // 保存値そのものが trim されている。
    let (status, resp) = send_request(
        app.clone(),
        "GET",
        &format!("/api/agents/{agent_id}/discord"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["owner_discord_id"], "987654321098765432");
}

