    /// 成功時のレスポンス JSON のキーと固定文言。
    #[tokio::test]
    async fn success_payload_shape_is_stable() {
        let db = db_with_agent();
        let d = FakeDelivery::new();
        let ctx = ctx_with_session();
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "555", "reviewer": "Crab B"}),
            &ctx,
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["channel_id"], "555");
        assert_eq!(data["parts"], 1);
        assert_eq!(data["task_id"], serde_json::Value::Null);
        assert_eq!(data["ledger_recorded"], false);
        assert_eq!(
            data["message"],
            "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。"
        );

        // ヘッダ + part 1/1 の 2 通、宛先はそのまま、メンションは transport の記法。
        let sent = d.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "555");
        assert!(sent[0]
            .1
            .starts_with("[Peer Review Request] <@42> from agent-a"));
        assert_eq!(sent[1].1, "part 1/1\ndiff");
    }

