    /// active タスクがあれば台帳へ `[peer review requested]` を記録し、`task_id` を返す。
    #[tokio::test]
    async fn records_the_request_in_the_task_ledger() {
        let db = db_with_agent();
        let d = FakeDelivery::new();
        let task_id = {
            let conn = db.lock().unwrap();
            opencrab_db::queries::insert_task_ledger(&conn, "agent-a", "sess-1", "goal", None)
                .unwrap()
        };
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "7", "instructions": "look here"}),
            &ctx_with_session(),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["task_id"], task_id);
        assert_eq!(data["ledger_recorded"], true);

        let conn = db.lock().unwrap();
        let progress = opencrab_db::queries::list_recent_task_progress(&conn, task_id, 10).unwrap();
        assert_eq!(progress.len(), 1);
        assert_eq!(
            progress[0].content,
            "[peer review requested] posted to channel 7 (1 parts) — focus: look here"
        );
    }

    /// **分割送信の途中失敗を明示する**（抽象越しに失われやすい情報。落とさない）。
    #[tokio::test]
    async fn partial_send_failure_reports_how_many_went_out() {
        let db = db_with_agent();
        // ヘッダ + 3 part = 4 通。3 通目（0-origin で 2）で失敗させる。
        let d = FakeDelivery::failing_at(2);
        let content = "あ".repeat(1900 * 2 + 10);
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": content, "channel_id": "9"}),
            &ctx_with_session(),
        )
        .await;
        assert!(!r.success);
        assert_eq!(
            r.error.unwrap(),
            "ピアレビュー依頼の送信に失敗（2/4 通送信済みの時点で失敗）: transport down。\
             投稿済みの依頼は不完全です。チャンネルに取り消しの一言を送ってから、必要なら再依頼してください。"
        );
        assert_eq!(d.count(), 2, "失敗前の 2 通だけが出ている");
    }

    /// 台帳記録は依頼が**未回収**であることの根拠になる（返信の自動記録ゲート）。
    /// 記録に失敗しても送信の成功は返す（best-effort）。
    #[tokio::test]
    async fn ledger_failure_does_not_fail_the_send() {
        let db = db_with_agent();
        let d = FakeDelivery::new();
        // task が無い（= 記録対象なし）ケース: ledger_recorded=false でも success。
        let r = request_peer_review(
            &db,
            &d,
            &json!({"content": "diff", "channel_id": "1"}),
            &ctx_with_session(),
        )
        .await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["ledger_recorded"], false);
        assert_eq!(d.count(), 2);
    }

