    fn insert_activity(conn: &rusqlite::Connection) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "agent".to_string(),
            agent_id: "a1".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: "https://discord.com/api/webhooks/1/tok".to_string(),
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

    #[test]
    fn test_webhook_tool_event_sink_preserves_shell_output_unredacted() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        };
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
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Completed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&result),
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("a batch should be sent");
        let msg = join_delivered(&batch.messages);
        assert!(msg.contains("tool_call_completed"));
        assert!(msg.contains("exit_code"));
        // covered 経路: stdout の secret はそのまま届く（masking しない）。
        assert!(
            msg.contains("API_KEY=supersecretvalue"),
            "secret stripped: {msg}"
        );
        assert!(!msg.contains("[REDACTED]"), "masking marker present: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_sends_failed_and_rejected() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity(&conn);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "denied" });
        let failed_result = serde_json::json!({
            "exit_code": 2,
            "stderr": "API_KEY=supersecretvalue failed",
            "truncated": false
        });
        let failed = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "failed-call",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Failed,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(5),
            args: &args,
            result: Some(&failed_result),
            error: Some("command failed"),
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &failed);
        let rejected = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "rejected-call",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Rejected,
            started_at: "2026-01-01T00:00:00Z",
            duration_ms: Some(1),
            args: &args,
            result: None,
            error: Some("permission denied"),
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &rejected);

        let failed_batch = rx.try_recv().expect("failed batch");
        let failed_msg = join_delivered(&failed_batch.messages);
        assert!(failed_msg.contains("tool_call_failed"));
        assert!(failed_msg.contains("exit_code"));
        // covered 経路: stderr の secret はそのまま届く（masking しない）。
        assert!(
            failed_msg.contains("API_KEY=supersecretvalue"),
            "secret stripped: {failed_msg}"
        );
        assert!(
            !failed_msg.contains("[REDACTED]"),
            "masking marker present: {failed_msg}"
        );
        let rejected_batch = rx.try_recv().expect("rejected batch");
        let rejected_msg = &rejected_batch.messages[0].content;
        assert!(rejected_msg.contains("tool_call_rejected"));
        assert!(rejected_msg.contains("permission denied"));
    }

    #[test]
    fn test_webhook_tool_event_sink_no_activity_row_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        };
        let args = serde_json::json!({});
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        assert!(rx.try_recv().is_err(), "no activity row -> nothing sent");
    }

    fn insert_activity_row(conn: &rusqlite::Connection, url: &str, enabled: bool) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: "agent".to_string(),
            agent_id: "a1".to_string(),
            tool_name: String::new(),
            kind: "activity".to_string(),
            url: url.to_string(),
            events_json: None,
            enabled,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    fn make_sink(
        db: opencrab_db::Db,
        tx: tokio::sync::mpsc::UnboundedSender<DeliveryBatch>,
    ) -> WebhookToolEventSink {
        WebhookToolEventSink {
            db,
            agent_id: "a1".to_string(),
            tx,
            max_chars: 1500,
            counter: AtomicUsize::new(0),
            cap: 200,
        }
    }

    fn started_event<'a>(args: &'a serde_json::Value) -> opencrab_actions::ToolEvent<'a> {
        opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: None,
            depth: 1,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args,
            result: None,
            error: None,
        }
    }

    // ---- tool/command argument inclusion on activity webhook messages ----

