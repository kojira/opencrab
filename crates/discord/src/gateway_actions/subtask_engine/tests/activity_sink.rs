    #[test]
    fn test_shared_worker_channel_preserves_order() {
        // 単一の共有 tx を使うと、先に送った lifecycle batch のあとに tool_call event が
        // 続き、FIFO 順序が保たれる（別 worker だと順序保証が崩れる）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();

        // lifecycle 相当の batch を共有 tx へ先に送る。
        tx.send(DeliveryBatch {
            url: "https://discord.com/api/webhooks/1/tok".to_string(),
            messages: vec!["lifecycle: started".into()],
        })
        .unwrap();

        // 同じ tx を使う sink から tool_call event を送る。
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({ "exit_code": 0, "stdout": "ok", "truncated": false });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "t",
            duration_ms: Some(1),
            args: &args,
            result: Some(&result),
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);

        // 受信順: lifecycle が先、tool_call が後。
        let first = rx.try_recv().expect("lifecycle batch");
        assert!(first.messages[0].content.contains("lifecycle: started"));
        let second = rx.try_recv().expect("tool_call batch");
        assert!(second.messages[0].content.contains("tool_call_completed"));
    }

    #[test]
    fn test_activity_diagnostic_batch_for_invalid_explicit_webhook_url() {
        // 非空の不正 explicit url は resolution Error を生み、その診断が activity default
        // へ redacted で配送されることを担保する。空 url はもはや Error にならない
        // （default へフォールバックする）ため、ここでは非空の不正 url を使う。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let args = serde_json::json!({
            "task": "do it",
            "webhook": { "url": "http://evil.example.com/api/webhooks/1/tok" }
        });
        let batch = build_activity_diagnostic_batch(
            &db,
            "a1",
            "spawn_subtask",
            "webhook_resolution_error",
            "spawn_subtask webhook resolution failed before execution: invalid_webhook_url: url must start with https:// (source: explicit)",
            &args,
        )
        .expect("diagnostic should route to activity default");
        assert_eq!(batch.url, "https://discord.com/api/webhooks/1/tok");
        let msg = &batch.messages[0].content;
        assert!(msg.contains("webhook_resolution_error"));
        assert!(msg.contains("invalid_webhook_url"));
        assert!(msg.contains("source: explicit"));
        assert!(!msg.contains("https://discord.com/api/webhooks/1/tok"));
    }

    // ---- depth0/main executor sink wiring (factory) ----

    /// activity 行が無いエージェントでは factory は None を返す（worker も起動しない）。
    #[tokio::test]
    async fn test_spawn_activity_sink_none_without_activity_row() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(sink.is_none(), "no activity row -> no sink");
    }

    /// activity 行があれば factory は Some を返し、その sink は depth0 イベントを
    /// activity webhook へ整形して配送する（covered 経路ゆえ unredacted で配送する）。
    #[tokio::test]
    async fn test_spawn_activity_sink_some_with_activity_row_and_delivers() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(sink.is_some(), "activity row -> sink present");

        // depth0 のツールイベントを流すと配送される（worker が実際に送ろうとするが、
        // ダミー URL なのでネットワークは best-effort で失敗する。ここでは on_event が
        // パニックせず整形できることを確認する）。
        let sink = sink.unwrap();
        let args = serde_json::json!({ "command": "echo hi" });
        let result = serde_json::json!({
            "exit_code": 0,
            "stdout": "leaked API_KEY=supersecretvalue here",
            "truncated": false
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&result),
            error: None,
        };
        sink.on_event(&ev);
    }

    fn insert_global_activity(conn: &rusqlite::Connection) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "global".to_string(),
            agent_id: "*".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: "https://discord.com/api/webhooks/9/glob".to_string(),
            events_json: None,
            enabled: true,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    /// global(`*`) のみの activity デフォルトでも factory は Some を返す
    /// （list_agent_webhook_config が agent_id='*' を含むため）。depth0 イベントが
    /// global 宛先へ stream され得ることを担保する。
    #[tokio::test]
    async fn test_spawn_activity_sink_some_with_global_only_activity_row() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_global_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        // agent "a1" 固有の行は無いが、global 行があるので Some。
        let sink = spawn_activity_tool_event_sink(db, "a1");
        assert!(
            sink.is_some(),
            "global-only activity default -> sink present"
        );
    }
