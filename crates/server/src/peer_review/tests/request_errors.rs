    // ---- 実体（引数検査・送信・台帳） ----

    fn db_with_agent() -> opencrab_db::Db {
        let db = opencrab_db::Db::memory().unwrap();
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::add_trusted_user(
                &conn,
                opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
                "row-1",
                "agent-a",
                "42",
                TrustedUserPermission::CoAgent,
                "owner",
                "2026-01-01",
                "Crab B",
            )
            .unwrap();
        }
        db
    }

    /// **エラー文言をリテラルで固定する**（移設でバイトが変わっていないこと）。
    #[tokio::test]
    async fn error_messages_are_byte_stable() {
        let db = db_with_agent();
        let d = FakeDelivery::new();

        // セッション必須（fail-closed）
        let no_session = GatewayCallContext::new(GatewayCaller::Agent, "agent-a");
        let r = request_peer_review(&db, &d, &json!({"content": "diff"}), &no_session).await;
        assert_eq!(
            r.error.unwrap(),
            "request_peer_review はセッション文脈でのみ実行できます（session_id 不明）"
        );
        // 空文字の session_id も同じく拒否する。
        let blank = GatewayCallContext::new(GatewayCaller::Agent, "agent-a").with_session_id("");
        let r = request_peer_review(&db, &d, &json!({"content": "diff"}), &blank).await;
        assert!(!r.success);

        let ctx = ctx_with_session();
        // content 未指定
        let r = request_peer_review(&db, &d, &json!({"channel_id": "123"}), &ctx).await;
        assert_eq!(
            r.error.unwrap(),
            "contentパラメータが必要です（レビュー対象のRAWコンテンツ）"
        );
        // 空白だけの content も未指定扱い
        let r =
            request_peer_review(&db, &d, &json!({"content": "  ", "channel_id": "1"}), &ctx).await;
        assert!(!r.success);

        // 長さ上限
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "x".repeat(12_001), "channel_id": "123"}),
            &ctx,
        )
        .await;
        assert_eq!(
            r.error.unwrap(),
            "contentが12000文字を超えています — ワークスペースにファイルとして保存し discord_send_file で添付した上で、contentには要点とファイル名を書いてください"
        );
        // 上限ちょうどは通る（境界）
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "x".repeat(12_000), "channel_id": "123"}),
            &ctx,
        )
        .await;
        assert!(r.success, "{:?}", r.error);

        // 宛先なし
        let r = request_peer_review(&db, &d, &json!({"content": "diff"}), &ctx).await;
        assert_eq!(
            r.error.unwrap(),
            "channel_idパラメータが必要です（実行文脈に返信先がありません）"
        );

        // 宛先が transport の形式に合わない（文言は transport が組む）
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "not-a-number"}),
            &ctx,
        )
        .await;
        assert_eq!(r.error.unwrap(), "無効なchannel_id: not-a-number");

        // レビュアー未登録（幻覚 id）
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "1", "reviewer": "999"}),
            &ctx,
        )
        .await;
        assert_eq!(
            r.error.unwrap(),
            "reviewer '999' が見つかりません。登録済みのピアレビュアー: Crab B (<@42>)"
        );
    }

