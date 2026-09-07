    #[tokio::test]
    async fn test_unknown_gateway_action() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute("nonexistent", &json!({}), &tctx(GatewayCaller::Agent))
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown gateway action"));
    }

    // ---- list_channels パラメータバリデーション ----

    #[tokio::test]
    async fn test_list_channels_missing_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_list_channels",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_list_channels_invalid_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_list_channels",
                &json!({"guild_id": "not-a-number"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("数値ID"));
    }

    // `create_skill` の 3 テスト（基本 / 同名 dedup / 非 trusted 拒否）は #157 S6 で
    // server 側（`crates/server/src/system_actions.rs`）へ移植済み（1 件も落としていない）。
    // 実体は `crates/server/src/agent_management.rs`。

    // ---- discord_send_file ----

    #[tokio::test]
    async fn test_send_file_workspace_violation() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_send_file",
                &json!({
                    "channel_id": "12345678901234567",
                    "file_path": "/etc/passwd",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(
            result.error.as_ref().unwrap().contains("ワークスペース外")
                || result.error.as_ref().unwrap().contains("見つかりません")
        );
    }

    #[tokio::test]
    async fn test_send_file_missing_params() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_send_file",
                &json!({"channel_id": "123"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("file_path"));
    }

    // ---- discord_create_channel ----

    #[test]
    fn test_create_channel_schema() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        let def = defs
            .iter()
            .find(|d| d.name == "discord_create_channel")
            .expect("discord_create_channel definition should exist");

        // required fields
        let required = def.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "guild_id"));
        assert!(required.iter().any(|v| v == "name"));

        // properties present
        let props = &def.parameters["properties"];
        for key in ["guild_id", "name", "parent_id", "topic", "reason"] {
            assert!(props.get(key).is_some(), "missing property {key}");
        }
    }

    #[tokio::test]
    async fn test_create_channel_missing_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"name": "general"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_create_channel_invalid_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"guild_id": "not-a-number", "name": "general"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("数値ID"));
    }

    #[tokio::test]
    async fn test_create_channel_missing_name() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"guild_id": "123456789"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("name"));
    }

    #[tokio::test]
    async fn test_create_channel_name_too_short() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({"guild_id": "123456789", "name": "a"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("2〜100文字"));
    }

    // ---- heartbeat instructions ----
    //
    // 4 テスト（owner 以外の拒否 + 監査なし / owner 成功 + 監査 / effective 解決 /
    // 素の agent 拒否 + co_agent 許可）は #157 S3 で server 側
    // （`crates/server/src/system_actions.rs`）へ移植済み。

    #[tokio::test]
    async fn test_create_channel_invalid_parent_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_create_channel",
                &json!({
                    "guild_id": "123456789",
                    "name": "general",
                    "parent_id": "not-a-number",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("parent_id"));
    }

    // ---- webhook 新規作成つきアクション（ensure_*） ----
    //
    // 可視化・管理の 6 本（`get/set_default_[subtask_]webhook` / `list_[subtask_]webhooks`）
    // とその 18 テストは #157 S5 で server 側（`crates/server/src/webhook_targets.rs`）へ
    // 移設済み。DB と設定ファイル由来の既定値しか触らないので gateway 非依存層で持てる。
    // ここに残るのは `discord_create_webhook`（serenity 依存）を呼ぶ `ensure_*` だけ。

    const WH_VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcSECRETtok";
    const WH_SECRET: &str = "abcSECRETtok";

