    // ---- #158 S1: 宛先の解決 ----

    /// #158 S1: 宛先を省略したら実行文脈の返信先（Discord では channel id の数値文字列）
    /// が使われる。
    #[test]
    fn resolve_channel_falls_back_to_ctx_reply_target() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("111222333".to_string()));
        let args = json!({"content": "diff"});
        let resolved = resolve_review_channel(&args, &ctx).unwrap();
        assert_eq!(resolved, "111222333");
    }

    /// #158 S1 非退行: 宛先を明示したら文脈の返信先より優先される（既定値が増えるだけ）。
    #[test]
    fn resolve_channel_prefers_explicit_argument() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("111222333".to_string()));
        let args = json!({"content": "diff", "channel_id": "999"});
        let resolved = resolve_review_channel(&args, &ctx).unwrap();
        assert_eq!(resolved, "999", "引数の宛先が文脈より優先される");
    }

    /// #158 S1: 引数も文脈も宛先を持たないなら "" で送らず明示エラー（fail-closed）。
    #[test]
    fn resolve_channel_fails_closed_without_any_target() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a");
        let args = json!({"content": "diff"});
        let err = resolve_review_channel(&args, &ctx).unwrap_err();
        assert!(
            err.starts_with("channel_idパラメータが必要です"),
            "既存の文言に揃える: {err}"
        );
    }

    /// 空文字は宛先として扱わない（"" のまま送らない = fail-closed の担保）。
    #[test]
    fn resolve_channel_treats_blank_as_unspecified() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("444".to_string()));
        let blank_args = json!({"content": "diff", "channel_id": "  "});
        let resolved = resolve_review_channel(&blank_args, &ctx).unwrap();
        assert_eq!(resolved, "444");

        let blank_ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a")
            .with_reply_target(Some("".to_string()));
        let args = json!({"content": "diff"});
        assert!(resolve_review_channel(&args, &blank_ctx).is_err());
    }

    /// 移設前は Discord gateway の `normalize_id_args` が JSON 数値の `*_id` を文字列化
    /// していた。合成 gateway にはその正規化が無いので、宛先解決側で同じ吸収を行う
    /// （モデルは channel_id を数値で渡してくることが多い）。
    #[test]
    fn resolve_channel_accepts_numeric_channel_id() {
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-a");
        // 2^53 を超えるスノーフレークでも精度を落とさない。
        let args = json!({"content": "diff", "channel_id": 1234567890123456789_u64});
        let resolved = resolve_review_channel(&args, &ctx).unwrap();
        assert_eq!(resolved, "1234567890123456789");
    }

