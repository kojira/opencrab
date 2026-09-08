    use super::*;
    use crate::system_actions::SystemGatewayActions;
    use opencrab_actions::webhook_target::WebhookConfig;
    use opencrab_gateway::GatewayActions;

    const WH_VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcSECRETtok";
    const WH_SECRET: &str = "abcSECRETtok";

    /// 応答 JSON に raw トークンが 1 度も現れないこと（秘匿処理の不変条件）。
    fn json_has_no_raw_token(v: &serde_json::Value) -> bool {
        !v.to_string().contains(WH_SECRET)
    }

    /// **transport 固有 gateway 無し**（`inner = None`）で合成 gateway を組む。
    ///
    /// これは web / REST / Nostr / heartbeat の経路、および Discord feature 無効ビルド
    /// そのもの。移設前はこの構成で 6 ツールが一切出なかった（#157 の不具合）。
    fn make_test_actions() -> (SystemGatewayActions, opencrab_db::Db) {
        make_test_actions_with_fallback(None)
    }

    /// 設定ファイル由来のフォールバックを注入した版。
    fn make_test_actions_with_fallback(
        default_subtask_webhook: Option<WebhookConfig>,
    ) -> (SystemGatewayActions, opencrab_db::Db) {
        let mut state = crate::test_app_state();
        state.default_subtask_webhook = default_subtask_webhook;
        let db = state.db.clone();
        (SystemGatewayActions::new(state, None, None, None), db)
    }

    /// テスト用の呼び出しコンテキスト（移設元の Discord テストと同じ agent_id）。
    fn tctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent")
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_requires_owner() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires owner"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_self_manage_allowed() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "family": "activity",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "agent self-manage should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert_eq!(data["enabled"], true);

        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap()
        .unwrap();
        assert!(row.enabled);
        assert_eq!(row.url, WH_VALID_URL);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_can_disable_own() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": "",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "agent disable should succeed: {:?}",
            result.error
        );
        assert_eq!(result.data.unwrap()["enabled"], false);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_tool_scope() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "tool",
                    "tool_name": "execute_shell",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_global() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "global",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_other_agent() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "agent_id": "someone-else",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_trusted_user_cannot_set() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires owner"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_owner_success_redacted() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner set should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert!(json_has_no_raw_token(&data), "raw token leaked in response");
        assert!(data["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));

        // stored in DB
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .unwrap();
        assert!(row.enabled);
        assert_eq!(row.url, WH_VALID_URL);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_invalid_url() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": "http://evil.com/x",
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("invalid_webhook_url"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_empty_url_disables() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .unwrap();
        assert!(!row.enabled);
    }

    #[tokio::test]
    async fn test_get_default_subtask_webhook_permission_and_redaction() {
        let (actions, db) = make_test_actions();
        // seed an agent default
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

        // bare agent denied
        let denied = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);

        // trusted_user allowed, redacted only
        let allowed = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(allowed.success);
        let data = allowed.data.unwrap();
        assert!(json_has_no_raw_token(&data));
        assert_eq!(data["status"], "ok");
        assert_eq!(data["source"], "agent_default");
    }

    #[tokio::test]
    async fn test_get_default_subtask_webhook_include_secret_rejected() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({  "include_secret": true }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("include_secret"));
    }

    #[tokio::test]
    async fn test_list_subtask_webhooks_permission_and_redaction() {
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

        // bare agent denied
        let denied = actions
            .execute(
                "list_subtask_webhooks",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);

        let allowed = actions
            .execute(
                "list_subtask_webhooks",
                &json!({}),
                &tctx(GatewayCaller::CoAgent {
                    agent_id: "co-agent-1".to_string(),
                }),
            )
            .await;
        assert!(allowed.success);
        let data = allowed.data.unwrap();
        assert!(json_has_no_raw_token(&data), "raw token leaked in list");
        let hooks = data["webhooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0]["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));
    }

