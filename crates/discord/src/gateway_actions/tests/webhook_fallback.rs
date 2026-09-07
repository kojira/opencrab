    /// 応答 JSON に raw トークンが 1 度も現れないこと（秘匿処理の不変条件）。
    fn json_has_no_raw_token(v: &serde_json::Value) -> bool {
        !v.to_string().contains(WH_SECRET)
    }

    #[tokio::test]
    async fn test_ensure_subtask_webhook_returns_existing_without_create() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            let row = opencrab_db::queries::AgentWebhookConfigRow {
                scope: "agent".to_string(),
                agent_id: "test-agent".to_string(),
                tool_name: String::new(),
                kind: "subtask".to_string(),
                url: WH_VALID_URL.to_string(),
                events_json: None,
                enabled: true,
                name: None,
                created_by: Some("owner".to_string()),
                output_mode: "summary".to_string(),
                max_chars: 1500,
                updated_at: String::new(),
            };
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row).unwrap();
        }
        // trusted_user can read existing without creating
        let result = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let data = result.data.unwrap();
        assert_eq!(data["created"], false);
        assert!(json_has_no_raw_token(&data));
    }

    #[tokio::test]
    async fn test_ensure_subtask_webhook_create_requires_owner_and_channel() {
        let (actions, _db) = make_test_actions();
        // non-owner, nothing exists -> owner-only error
        let non_owner = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!non_owner.success);
        assert!(non_owner.error.unwrap().contains("owner"));

        // owner but no channel_id -> channel_id required
        let no_channel = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!no_channel.success);
        assert!(no_channel.error.unwrap().contains("channel_id"));
    }

    /// **設定ファイル由来のフォールバックが Discord 経路で今までどおり効く**（#157 S5）。
    ///
    /// この値は #157 S5 で `AppState` へ持ち上げたが、Discord gateway_actions は
    /// 従来どおりコンストラクタで同じ値を受け取る。DB に行が無くても既定が解決され、
    /// `ensure_*` は webhook を**作らずに**それを返す（持ち上げによる挙動変化なし）。
    #[tokio::test]
    async fn config_fallback_still_resolves_on_the_discord_path() {
        let db = opencrab_db::Db::memory().unwrap();
        let http = Arc::new(Http::new("dummy-token"));
        let actions = DiscordGatewayActions::new(
            http,
            db,
            "/tmp".to_string(),
            opencrab_actions::webhook_target::WebhookConfig::from_parts(
                WH_VALID_URL.to_string(),
                Some(vec!["started".to_string()]),
            ),
        );

        let result = actions
            .execute(
                "ensure_subtask_webhook",
                &json!({ "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let data = result.data.unwrap();
        assert_eq!(data["created"], false, "既定があるので作成してはいけない");
        assert_eq!(data["source"], "env_config");
        assert_eq!(data["scope"], "env_config");
        assert!(json_has_no_raw_token(&data));
    }
