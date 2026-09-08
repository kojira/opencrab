    // ---- #157 S5 で新規に固定する不変条件 ----

    /// **設定ファイル由来のフォールバックが Discord 以外の経路でも効く**（持ち上げの証明）。
    ///
    /// `inner = None`（transport 固有 gateway 無し = web / REST / Nostr / heartbeat、
    /// および Discord feature 無効ビルド）で、DB に行が 1 つも無い状態。持ち上げ前は
    /// この値へ到達できず `status: "none"` になっていた。
    #[tokio::test]
    async fn config_fallback_resolves_without_any_transport_gateway() {
        let (actions, _db) = make_test_actions_with_fallback(WebhookConfig::from_parts(
            WH_VALID_URL.to_string(),
            Some(vec!["started".to_string()]),
        ));
        let result = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let data = result.data.unwrap();
        assert_eq!(data["status"], "ok");
        assert_eq!(data["source"], "env_config");
        assert_eq!(data["scope"], "env_config");
        assert_eq!(data["family"], "subtask");
        assert_eq!(data["events"], json!(["started"]));
        assert!(
            json_has_no_raw_token(&data),
            "設定由来のフォールバックでも raw トークンを返さない"
        );
    }

    /// フォールバック未設定なら `status: "none"`（移設前と同じ「無い」応答）。
    #[tokio::test]
    async fn missing_config_fallback_still_reports_none() {
        let (actions, _db) = make_test_actions();
        let data = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .data
            .unwrap();
        assert_eq!(data["status"], "none");
        assert_eq!(data["source"], serde_json::Value::Null);
    }

    /// activity family は設定ファイル由来のフォールバックを**使わない**（移設前と同じ）。
    ///
    /// `resolve_activity_webhook` は activity kind の DB 行しか見ない。持ち上げで
    /// うっかり activity にも効かせてしまうと、通知先を設定していないエージェントの
    /// ツール活動が全部その webhook へ流れる。
    #[tokio::test]
    async fn config_fallback_does_not_leak_into_the_activity_family() {
        let (actions, _db) = make_test_actions_with_fallback(WebhookConfig::from_parts(
            WH_VALID_URL.to_string(),
            None,
        ));
        let data = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .data
            .unwrap();
        assert_eq!(data["status"], "none", "activity は env_config を使わない");
        assert_eq!(data["family"], "activity");
    }

    /// **エラー文言をバイト単位で固定する**（移設で 1 文字も変わっていないことの防波堤）。
    ///
    /// `contains` ではなく完全一致で比較する。拒否には構造マーカー
    /// （`REJECTION_CODE_PREFIX`）が付き、通常の失敗には付かないことも併せて固定する。
    #[tokio::test]
    async fn error_messages_are_byte_for_byte_unchanged() {
        let (actions, _db) = make_test_actions();

        // 読み取り権限（拒否 = マーカー付き）
        for name in [
            "get_default_subtask_webhook",
            "get_default_webhook",
            "list_subtask_webhooks",
            "list_webhooks",
        ] {
            let e = actions
                .execute(name, &json!({}), &tctx(GatewayCaller::Agent))
                .await
                .error
                .unwrap();
            assert_eq!(
                e,
                format!(
                    "{REJECTION_CODE_PREFIX}redacted read requires owner/trusted_user/co_agent"
                ),
                "{name}"
            );
        }

        // set の権限拒否 3 種（いずれもマーカー付き）
        let e = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({"scope": "agent", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await
            .error
            .unwrap();
        assert_eq!(
            e,
            format!(
                "{REJECTION_CODE_PREFIX}set/disable requires owner (agents may manage only their own agent scope)"
            )
        );

        let e = actions
            .execute(
                "set_default_webhook",
                &json!({"scope": "global", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::Agent),
            )
            .await
            .error
            .unwrap();
        assert_eq!(
            e,
            format!(
                "{REJECTION_CODE_PREFIX}forbidden_scope: an agent may only set/disable its own agent-scope default webhook"
            )
        );

        let e = actions
            .execute(
                "set_default_webhook",
                &json!({"scope": "agent", "agent_id": "other", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::Agent),
            )
            .await
            .error
            .unwrap();
        assert_eq!(
            e,
            format!(
                "{REJECTION_CODE_PREFIX}forbidden_scope: an agent may only set/disable its own agent default webhook"
            )
        );

        // 引数エラー（拒否ではないのでマーカーは付かない）
        let e = actions
            .execute(
                "set_default_webhook",
                &json!({"url": WH_VALID_URL}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .error
            .unwrap();
        assert_eq!(e, "scope is required: 'agent' | 'tool' | 'global'");
        assert!(!e.starts_with(REJECTION_CODE_PREFIX));

        let e = actions
            .execute(
                "get_default_webhook",
                &json!({"include_secret": true}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .error
            .unwrap();
        assert_eq!(e, "include_secret is not supported");
        assert!(!e.starts_with(REJECTION_CODE_PREFIX));
    }

    /// **URL のホスト許可リストを緩めていない**。
    ///
    /// `validate_webhook_url` は下位層（#157 S4）に降りているが、set の入口でそれを
    /// 通していることをここで固定する。文言も移設前と同一。
    #[tokio::test]
    async fn host_allowlist_is_still_enforced_on_set() {
        let (actions, db) = make_test_actions();
        for bad in [
            "https://evil.example.com/api/webhooks/1/tok",
            "http://discord.com/api/webhooks/1/tok",
            "https://discord.com/not-a-webhook",
        ] {
            let result = actions
                .execute(
                    "set_default_webhook",
                    &json!({"scope": "agent", "url": bad}),
                    &tctx(GatewayCaller::Owner),
                )
                .await;
            assert!(!result.success, "{bad} が通ってしまう");
            let e = result.error.unwrap();
            assert!(
                e.starts_with("invalid_webhook_url: "),
                "{bad}: 文言が変わっている: {e}"
            );
        }
        // 拒否された URL は 1 行も保存されていない。
        let conn = db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_webhook_config(&conn, Some("test-agent"), true)
            .unwrap();
        assert!(
            rows.is_empty(),
            "検証に落ちた URL が保存されている: {rows:?}"
        );
    }

    /// **成功応答の JSON キー集合**を固定する（移設で増減していないこと）。
    #[tokio::test]
    async fn success_response_json_keys_are_unchanged() {
        let (actions, _db) = make_test_actions();

        let set = actions
            .execute(
                "set_default_webhook",
                &json!({"scope": "agent", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let mut keys: Vec<&String> = set
            .data
            .as_ref()
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agent_id",
                "enabled",
                "family",
                "redacted_url",
                "scope",
                "tool_name"
            ]
        );
        assert!(json_has_no_raw_token(set.data.as_ref().unwrap()));

        let get = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let mut keys: Vec<&String> = get
            .data
            .as_ref()
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "enabled",
                "events",
                "family",
                "redacted_url",
                "scope",
                "source",
                "status"
            ]
        );

        let list = actions
            .execute("list_webhooks", &json!({}), &tctx(GatewayCaller::Owner))
            .await;
        let items = list.data.as_ref().unwrap()["webhooks"].as_array().unwrap();
        let mut keys: Vec<&String> = items[0].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agent_id",
                "enabled",
                "events",
                "kind",
                "max_chars",
                "name",
                "output_mode",
                "redacted_url",
                "scope",
                "tool_name",
                "updated_at"
            ]
        );
        assert!(json_has_no_raw_token(list.data.as_ref().unwrap()));
    }
