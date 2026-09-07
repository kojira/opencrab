    /// 汎用 set_default_webhook は既定で family='activity' の行を upsert する。
    #[tokio::test]
    async fn test_generic_set_default_webhook_defaults_to_activity_family() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner set should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert_eq!(data["family"], "activity");
        assert!(json_has_no_raw_token(&data), "raw token leaked in response");

        let conn = db.lock().unwrap();
        // activity 行が作られ、subtask 行は作られない。
        let activity = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap();
        assert!(activity.is_some(), "activity row should exist");
        assert_eq!(activity.unwrap().url, WH_VALID_URL);
        let subtask = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap();
        assert!(
            subtask.is_none(),
            "subtask row must not be created by generic name"
        );
    }

    /// 後方互換 set_default_subtask_webhook は既定で family='subtask' を返しつつ、
    /// agent の通常 tool/command activity へも効くよう activity 行も mirror する。
    #[tokio::test]
    async fn test_subtask_named_set_defaults_to_subtask_and_activity_families() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.data.unwrap()["family"], "subtask");
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .is_some());
        let activity = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap();
        assert!(
            activity.is_some(),
            "compat subtask default should also enable activity streaming"
        );
        let resolved = opencrab_actions::webhook_target::resolve_activity_webhook(
            &conn,
            "test-agent",
            "execute_shell",
        );
        assert!(
            matches!(
                resolved,
                opencrab_actions::webhook_target::WebhookResolution::Use { .. }
            ),
            "activity default should resolve after set_default_subtask_webhook"
        );
    }

    /// agent 自身は汎用名でも自分の agent-scope のみ設定でき、他 scope は拒否される。
    #[tokio::test]
    async fn test_generic_set_default_webhook_agent_scope_permission() {
        let (actions, _db) = make_test_actions();
        // 自分の agent-scope は許可。
        let ok = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            ok.success,
            "agent self-manage should succeed: {:?}",
            ok.error
        );
        // global は拒否。
        let denied = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "global", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);
        assert!(denied.error.unwrap().contains("forbidden_scope"));
    }

    /// 汎用 get_default_webhook は activity 行のみを解決する（subtask 行は使わない）。
    #[tokio::test]
    async fn test_generic_get_default_webhook_resolves_activity_only() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            // subtask 行のみを seed。activity 行は無い。
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
        // activity family の解決では subtask 行に fall through しない → none。
        let activity = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(activity.success);
        let data = activity.data.unwrap();
        assert_eq!(data["status"], "none");
        assert_eq!(data["family"], "activity");
        // subtask family（family 明示）なら解決できる。
        let subtask = actions
            .execute(
                "get_default_webhook",
                &json!({  "family": "subtask" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert_eq!(subtask.data.unwrap()["status"], "ok");
    }

    /// 汎用 list_webhooks は family で kind を絞り込める。
    #[tokio::test]
    async fn test_generic_list_webhooks_family_filter() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            for kind in ["subtask", "activity"] {
                let row = opencrab_db::queries::AgentWebhookConfigRow {
                    scope: "agent".to_string(),
                    agent_id: "test-agent".to_string(),
                    tool_name: String::new(),
                    kind: kind.to_string(),
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
        }
        // 絞り込み無し → 両方。
        let all = actions
            .execute("list_webhooks", &json!({}), &tctx(GatewayCaller::Owner))
            .await;
        assert_eq!(all.data.unwrap()["webhooks"].as_array().unwrap().len(), 2);
        // family=activity → 1 件。
        let filtered = actions
            .execute(
                "list_webhooks",
                &json!({  "family": "activity" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let hooks = filtered.data.unwrap();
        let arr = hooks["webhooks"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "activity");
        assert!(json_has_no_raw_token(&hooks));
    }

