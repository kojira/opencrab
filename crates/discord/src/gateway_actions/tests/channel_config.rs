    #[tokio::test]
    async fn test_channel_config_upsert() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(result.success);
        assert!(result.error.is_none());

        let data = result.data.unwrap();
        assert_eq!(data["channel_id"], "ch-1");
        assert_eq!(data["readable"], true);
        assert_eq!(data["writable"], false);

        // DB確認
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(cfg.readable);
        assert!(!cfg.writable);
        assert_eq!(cfg.channel_name, "general");
        assert_eq!(cfg.guild_id, "guild-1");
    }

    #[tokio::test]
    async fn test_channel_config_update_existing() {
        let (actions, db) = make_test_actions();

        // 初回設定
        actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;

        // 更新
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": false,
                    "writable": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(!cfg.readable);
        assert!(!cfg.writable);
    }

    /// #421: whitelisted / heartbeat_enabled を省略した更新は既存値を保持する（patch 意味論）。
    /// full-replace のまま既定値へ落とすと、読み書きだけ変える操作で既存の whitelist が
    /// 黙って消え、そのエージェントがそのチャンネルで無言破棄されるようになる。
    #[tokio::test]
    async fn test_channel_config_omitted_fields_preserve_existing() {
        let (actions, db) = make_test_actions();

        // whitelisted=true, heartbeat 無効 で初期化。
        let r1 = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": true,
                    "whitelisted": true,
                    "heartbeat_enabled": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(r1.success);

        // 読み書きだけ変える意図で whitelisted / heartbeat_enabled を省略して更新。
        let r2 = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": false,
                    "writable": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(r2.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        // 明示した値は反映される。
        assert!(!cfg.readable);
        assert!(!cfg.writable);
        // 省略した値は既存を保持する（ここが壊れると #421 の無言 whitelist 消滅が再発）。
        assert!(
            cfg.whitelisted,
            "omitted whitelisted must preserve existing 1"
        );
        assert!(
            !cfg.heartbeat_enabled,
            "omitted heartbeat_enabled must preserve existing false"
        );
    }

    /// #421: 明示指定した値は従来どおりそのまま書く（省略時保持で常に据え置きにはしない）。
    #[tokio::test]
    async fn test_channel_config_explicit_whitelisted_false_overrides_existing() {
        let (actions, db) = make_test_actions();

        // 既存 whitelisted=true。
        actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "readable": true,
                    "writable": true,
                    "whitelisted": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;

        // 明示的に false へ。
        let r = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "readable": true,
                    "writable": true,
                    "whitelisted": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(r.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert!(
            !cfg.whitelisted,
            "explicit whitelisted=false must be written"
        );
    }

    #[tokio::test]
    async fn test_channel_config_missing_params() {
        let (actions, _db) = make_test_actions();

        // channel_idのみ → guild_idが欠けてエラー
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({"channel_id": "ch-1"}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_channel_config_missing_readable() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "writable": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("readable"));
    }

    #[tokio::test]
    async fn test_channel_config_optional_name() {
        let (actions, db) = make_test_actions();
        // channel_nameなしでも動く
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "readable": true,
                    "writable": true,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "test-agent")
            .unwrap()
            .unwrap();
        assert_eq!(cfg.channel_name, "");
    }

    /// 回帰: モデルが Discord スノーフレーク ID を JSON 数値で渡しても通ること。
    /// 以前は各ハンドラが as_str だけを見ており「channel_id パラメータが必要です」と
    /// 誤って失敗していた（2^53 超の 19 桁 ID も精度を保って文字列化される）。
    #[tokio::test]
    async fn test_channel_config_numeric_ids() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": 444455556666777788u64,
                    "guild_id": 222233334444555566u64,
                    "readable": true,
                    "writable": true,
                    "whitelisted": false,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "numeric ids should be accepted: {:?}",
            result.error
        );

        // DB には文字列化した ID が精度そのままで入る。
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(
            &conn,
            "444455556666777788",
            "test-agent",
        )
        .unwrap()
        .unwrap();
        assert!(cfg.readable);
        assert!(cfg.writable);
        assert_eq!(cfg.guild_id, "222233334444555566");
    }

    #[test]
    fn test_normalize_id_args_stringifies_only_id_numbers() {
        // *_id の整数は文字列化、それ以外（真偽・非 id 数値・既に文字列）は不変。
        let input = json!({
            "channel_id": 444455556666777788u64,
            "guild_id": "already-str",
            "readable": true,
            "count": 5,
        });
        let out = normalize_id_args(&input);
        assert_eq!(out["channel_id"], json!("444455556666777788"));
        assert_eq!(out["guild_id"], json!("already-str"));
        assert_eq!(out["readable"], json!(true));
        assert_eq!(out["count"], json!(5));

        // 変換不要ならコピーしない（借用のまま）。
        let noop = json!({"readable": true, "count": 1});
        assert!(matches!(normalize_id_args(&noop), Cow::Borrowed(_)));
    }

    // ---- unknown action ----

