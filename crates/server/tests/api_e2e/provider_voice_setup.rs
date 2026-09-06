// ==================== Provider Settings (dashboard) ====================

#[tokio::test]
async fn test_list_llm_providers() {
    let app = create_test_app();
    let (status, json) = send_request(app, "GET", "/api/llm/providers", None).await;
    assert_eq!(status, StatusCode::OK);
    let providers = json["providers"].as_array().unwrap();
    // 既知プロバイダーは TOML/DB に無くても列挙される
    let names: Vec<&str> = providers
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"ollama"));
    // 未設定なのでキーは none / 非稼働
    let openai = providers.iter().find(|p| p["name"] == "openai").unwrap();
    assert_eq!(openai["api_key_source"], "none");
    assert_eq!(openai["active"], false);
}

#[tokio::test]
async fn test_update_provider_sets_key_and_reloads() {
    let app = create_test_app();
    // API キーを設定 → ルーター再構築で openai が稼働状態になる
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({"api_key": "sk-test-dashboard-key", "default_model": "gpt-4o"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["reloaded"], true);
    let p = &json["provider"];
    assert_eq!(
        p["active"], true,
        "provider should be live after key set: {p}"
    );
    assert_eq!(p["api_key_source"], "db");
    // 平文キーは応答に含まれない（マスクのみ）
    assert!(!json.to_string().contains("sk-test-dashboard-key"));
    assert_eq!(p["api_key_masked"], "••••-key");

    // オーバーライド削除 → 非稼働に戻る
    let (status, json) = send_request(
        app.clone(),
        "DELETE",
        "/api/llm/providers/openai/override",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (_, json) = send_request(app, "GET", "/api/llm/providers", None).await;
    let openai = json["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "openai")
        .unwrap()
        .clone();
    assert_eq!(openai["active"], false);
    assert_eq!(openai["has_override"], false);
}

#[tokio::test]
async fn test_update_provider_disable_and_reject_bad_name() {
    let app = create_test_app();
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/ollama",
        Some(serde_json::json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["provider"]["enabled_override"], false);

    // 不正なプロバイダー名は 400
    let (status, _) = send_request(
        app,
        "PUT",
        "/api/llm/providers/bad%2Fname",
        Some(serde_json::json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_agent_reasoning_effort_patch_roundtrip() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    // 既定は未設定（null）
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp["reasoning_effort"].is_null());

    // PATCH で設定
    let (status, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({ "reasoning_effort": "high" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, resp) =
        send_request(app.clone(), "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert_eq!(resp["reasoning_effort"], "high");

    // 空文字で解除 → NULL に正規化される（null は serde の都合でクリア不可のため）
    let (_, _) = send_request(
        app.clone(),
        "PATCH",
        &format!("/api/agents/{agent_id}"),
        Some(serde_json::json!({ "reasoning_effort": "" })),
    )
    .await;
    let (_, resp) = send_request(app, "GET", &format!("/api/agents/{agent_id}"), None).await;
    assert!(resp["reasoning_effort"].is_null());
}

#[tokio::test]
async fn test_codex_diagnostics_returns_fields() {
    let app = create_test_app();
    let (status, json) = send_request(app, "GET", "/api/llm/codex/diagnostics", None).await;
    assert_eq!(status, StatusCode::OK);
    // テスト設定には codex プロバイダーが無いので configured_path は既定の "codex"
    assert_eq!(json["configured_path"], "codex");
    // version/resolved_path/error のキーが存在すること（値は環境依存）
    assert!(json.get("version").is_some());
    assert!(json.get("resolved_path").is_some());
    assert!(json.get("error").is_some());
}

#[tokio::test]
async fn test_update_provider_reasoning_effort_roundtrip() {
    let app = create_test_app();
    // 推論強度を設定
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/codex",
        Some(serde_json::json!({ "reasoning_effort": "medium" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["provider"]["reasoning_effort"], "medium");
    assert_eq!(json["provider"]["has_override"], true);

    // GET でも反映
    let (_, json) = send_request(app.clone(), "GET", "/api/llm/providers", None).await;
    let codex = json["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "codex")
        .unwrap()
        .clone();
    assert_eq!(codex["reasoning_effort"], "medium");

    // null で解除 → モデル既定（空）に戻り、他フィールドが無ければ行ごと消える
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/codex",
        Some(serde_json::json!({ "reasoning_effort": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["provider"]["reasoning_effort"], "");
    assert_eq!(json["provider"]["has_override"], false);
}

#[tokio::test]
async fn test_update_provider_null_clears_field_keeps_others() {
    let app = create_test_app();
    // まずキーと無効化を両方設定
    let (status, _) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({"api_key": "sk-keep-me", "enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 三値: enabled:null は「無効化を解除」= TOML に戻す。api_key は維持されること。
    // （旧実装では serde が null を None に潰し、この解除が無反応だった）
    let (status, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({ "enabled": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let p = &json["provider"];
    assert_eq!(
        p["enabled_override"],
        serde_json::Value::Null,
        "enabled must be cleared"
    );
    // キー設定は維持 → 稼働状態のまま
    assert_eq!(p["api_key_source"], "db");
    assert_eq!(p["active"], true);
    assert_eq!(p["has_override"], true);

    // base_url:null は base_url オーバーライドだけを消す（キーは残る）
    let (_, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({ "base_url": "https://x.example" })),
    )
    .await;
    assert_eq!(json["provider"]["base_url"], "https://x.example");
    let (_, json) = send_request(
        app.clone(),
        "PUT",
        "/api/llm/providers/openai",
        Some(serde_json::json!({ "base_url": null })),
    )
    .await;
    assert_eq!(json["provider"]["base_url"], "");
    assert_eq!(
        json["provider"]["api_key_source"], "db",
        "key must survive base_url clear"
    );
}

#[tokio::test]
async fn test_voice_config_invalid_provider_not_persisted() {
    let app = create_test_app();
    // enabled + 未知の STT プロバイダ → 400、かつ DB に保存されないこと
    let bad = serde_json::json!({
        "enabled": true,
        "stt": { "provider": "nonexistent", "model": "x", "api_key_env": "X" },
        "tts": { "provider": "voicevox", "default_voice": "3" }
    });
    let (status, _) = send_request(app.clone(), "PUT", "/api/voice/config", Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // GET は依然 TOML 由来（壊れた値が db として残っていない）
    let (_, json) = send_request(app, "GET", "/api/voice/config", None).await;
    assert_eq!(json["source"], "toml");
    assert_eq!(json["config"]["enabled"], false);
}

#[tokio::test]
async fn test_voice_config_roundtrip() {
    let app = create_test_app();
    // 初期状態: TOML 由来（テストでは Default = disabled）
    let (status, json) = send_request(app.clone(), "GET", "/api/voice/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["source"], "toml");
    assert_eq!(json["config"]["enabled"], false);
    assert_eq!(json["runtime_active"], false);

    // 保存（ランタイム停止中なので restart_required）
    let mut config = json["config"].clone();
    config["enabled"] = serde_json::json!(true);
    config["tts"]["default_voice"] = serde_json::json!("1");
    let (status, json) = send_request(app.clone(), "PUT", "/api/voice/config", Some(config)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["saved"], true);
    assert_eq!(json["applied_live"], false);
    assert_eq!(json["restart_required"], true);

    // 読み直すと DB 由来になっている
    let (_, json) = send_request(app.clone(), "GET", "/api/voice/config", None).await;
    assert_eq!(json["source"], "db");
    assert_eq!(json["config"]["tts"]["default_voice"], "1");

    // リセットで TOML に戻る
    let (status, _) = send_request(app.clone(), "DELETE", "/api/voice/config", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_, json) = send_request(app, "GET", "/api/voice/config", None).await;
    assert_eq!(json["source"], "toml");
}

// ==================== Onboarding / Setup ====================

#[tokio::test]
async fn test_setup_status_fresh_and_after_agent() {
    let app = create_test_app();

    // フレッシュ DB + プロバイダ無しのルーター: 全ステップ未完。
    let (status, json) = send_request(app.clone(), "GET", "/api/setup/status", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["complete"], false);
    assert_eq!(json["next_step"], "llm_provider");
    assert_eq!(json["steps"]["llm_provider"]["done"], false);
    assert_eq!(json["steps"]["agent"]["done"], false);
    assert_eq!(json["steps"]["agent"]["count"], 0);
    assert_eq!(json["steps"]["discord"]["done"], false);
    assert_eq!(json["steps"]["channel"]["done"], false);

    // エージェントを作ると agent ステップが done + count=1 になる。
    let (_agent_id, app) = create_test_agent(app).await;
    let (status, json) = send_request(app, "GET", "/api/setup/status", None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["steps"]["agent"]["done"], true);
    assert_eq!(json["steps"]["agent"]["count"], 1);
    // LLM が未設定なので next_step は依然 llm_provider。
    assert_eq!(json["next_step"], "llm_provider");
}

#[tokio::test]
async fn test_setup_seed_standard_skills() {
    let app = create_test_app();
    let (agent_id, app) = create_test_agent(app).await;

    // OPENCRAB_SKILLS_DIR を一時ディレクトリに向け、1 件のスキルファイルを置く。
    let dir = std::env::temp_dir().join(format!("opencrab-seed-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("demo.skill.md"),
        "---\nname: demo\ndescription: \"デモスキル\"\nversion: 1\npermission: agent\nactions:\n  - send_speech\n---\n\n# デモ\n\nガイダンス。\n",
    )
    .unwrap();
    // テスト内でのみ使う（この 2 テストは env を共有しないよう別ディレクトリ）。
    std::env::set_var("OPENCRAB_SKILLS_DIR", &dir);

    let (status, json) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills/seed-standard"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["seeded_count"], 1);
    assert_eq!(json["seeded"][0], "demo");

    // 2 回目は冪等（同名スキップ）。
    let (status, json) = send_request(
        app.clone(),
        "POST",
        &format!("/api/agents/{agent_id}/skills/seed-standard"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["seeded_count"], 0);
    assert_eq!(json["skipped"][0], "demo");

    // シードしたスキルが一覧に出る。
    let (_, json) = send_request(app, "GET", &format!("/api/agents/{agent_id}/skills"), None).await;
    let names: Vec<String> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(names.contains(&"demo".to_string()), "skills: {names:?}");

    std::env::remove_var("OPENCRAB_SKILLS_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

