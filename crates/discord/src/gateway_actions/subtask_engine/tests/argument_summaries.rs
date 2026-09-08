    #[test]
    fn test_webhook_tool_event_sink_started_includes_command_args() {
        // started イベントにコマンド引数が含まれること（depth0 を想定）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "command": "git status --short" });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0].content;
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("args:"), "args line missing: {msg}");
        assert!(msg.contains("git status --short"), "command missing: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_started_includes_command_and_args_array() {
        // E2E 再現: command `echo` と args `["hello","webhook-args-test"]` が
        // started イベントで両方描画されること（args 配列が欠落しない）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({
            "command": "echo",
            "args": ["hello", "webhook-args-test"]
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0].content;
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("echo"), "command missing: {msg}");
        assert!(msg.contains("hello"), "first arg missing: {msg}");
        assert!(
            msg.contains("webhook-args-test"),
            "second arg missing: {msg}"
        );
    }

    #[test]
    fn test_webhook_tool_event_sink_started_includes_non_shell_args() {
        // 非 shell ツールでも started に引数（JSON）が含まれる。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({ "path": "notes/todo.md", "limit": 10 });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "read_file",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = &batch.messages[0].content;
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        assert!(msg.contains("notes/todo.md"), "args missing: {msg}");
    }

    #[test]
    fn test_webhook_tool_event_sink_started_preserves_secret_args_unredacted() {
        // covered 経路: started の引数に含まれるシークレット（API キー / Discord webhook
        // URL）も masking せずそのまま届く（新要件 §2 P4 / AC4）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer sk-supersecretkeyvalue1234' https://discord.com/api/webhooks/999/leakedtokenvalue && export API_KEY=anothersupersecretvalue"
        });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        let msg = join_delivered(&batch.messages);
        assert!(msg.contains("tool_call_started"), "msg: {msg}");
        // every secret-like token survives unmodified, and no masking markers appear.
        assert!(
            !msg.contains("[REDACTED]"),
            "REDACTED marker present: {msg}"
        );
        assert!(
            !msg.contains("[redacted]"),
            "redacted marker present: {msg}"
        );
        assert!(
            msg.contains("sk-supersecretkeyvalue1234"),
            "api key stripped: {msg}"
        );
        assert!(
            msg.contains("https://discord.com/api/webhooks/999/leakedtokenvalue"),
            "webhook url stripped: {msg}"
        );
        assert!(
            msg.contains("API_KEY=anothersupersecretvalue"),
            "API_KEY value stripped: {msg}"
        );
    }

    #[test]
    fn test_webhook_tool_event_sink_long_args_go_out_as_one_attachment() {
        // #293: 長大な引数はクランプ（…）も part X/N 連投もせず、**1 通**の
        // 「プレビュー + 全文添付」で出る。全文は添付側にロスなく入る。
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let long_cmd = format!("echo {}", "word ".repeat(2_000));
        let args = serde_json::json!({ "command": long_cmd });
        let ev = opencrab_actions::ToolEvent {
            tool_name: "execute_shell",
            tool_call_id: "c1",
            agent_id: "a1",
            session_id: Some("s1"),
            depth: 0,
            status: opencrab_actions::ToolEventStatus::Started,
            started_at: "t",
            duration_ms: None,
            args: &args,
            result: None,
            error: None,
        };
        opencrab_actions::ToolEventSink::on_event(&sink, &ev);
        let batch = rx.try_recv().expect("started batch should be sent");
        assert_eq!(batch.messages.len(), 1, "長文でも連投せず 1 通のはず");
        let m = &batch.messages[0];
        assert!(m.has_attachment(), "全文は添付になるはず");
        assert!(
            m.content.chars().count() <= 2000,
            "preview exceeds Discord limit: {}",
            m.content.chars().count()
        );
        assert!(
            !m.content.starts_with("part 1/"),
            "part framing must be gone: {}",
            m.content
        );
        // 添付本体 -> 2000 個の 'word' が欠けずに入っている。
        let full = m.delivered_text();
        assert_eq!(full.matches("word").count(), 2_000, "lost args");
        let att = m.attachment.as_ref().unwrap();
        assert_eq!(att.filename, "tool_call_started-execute_shell.txt");
        assert_eq!(att.content_type, "text/plain; charset=utf-8");
        assert!(!att.truncated);
    }

    // ---- summarize_tool_args: unredacted, lossless ----

    #[test]
    fn test_summarize_tool_args_preserves_secrets_unredacted() {
        // covered 経路: 引数中の secret も masking/クランプせずそのまま残す。
        let secret = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd"; // 40 文字の英数字
        let prefix = "a ".repeat(145); // 290 文字
        let cmd = format!("{prefix}{secret}");
        let args = serde_json::json!({ "command": cmd });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.starts_with("cmd: `"), "summary: {summary}");
        assert!(summary.contains(secret), "secret stripped: {summary}");
        assert!(
            !summary.contains("[REDACTED]"),
            "masking marker present: {summary}"
        );
        assert!(
            !summary.contains('…'),
            "clamp ellipsis introduced: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_preserves_webhook_url() {
        // /api/webhooks/ を含む URL がそのまま残ること（バイト一致）。
        let url =
            "https://discord.com/api/webhooks/123456789012345678/AbCdEf-XXXXXXXXXXXXXXXXXXXXXXXX";
        let args = serde_json::json!({ "command": format!("curl {url}") });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains(url), "webhook url stripped: {summary}");
        assert!(
            !summary.contains("[redacted]"),
            "url masking present: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_execute_shell_includes_command_and_args() {
        // execute_shell の実引数（command + args 配列）が両方描画されること。
        let args = serde_json::json!({
            "command": "echo",
            "args": ["hello", "webhook-args-test"]
        });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains("echo"), "command missing: {summary}");
        assert!(summary.contains("hello"), "first arg missing: {summary}");
        assert!(
            summary.contains("webhook-args-test"),
            "second arg missing: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_execute_shell_marks_stdin_without_leaking() {
        // stdin は本文を出さず、存在とバイト数のみ示す。
        let args = serde_json::json!({
            "command": "cat",
            "stdin": "secret-stdin-body"
        });
        let summary = summarize_tool_args("execute_shell", &args).unwrap();
        assert!(summary.contains("cat"), "command missing: {summary}");
        assert!(summary.contains("stdin"), "stdin marker missing: {summary}");
        assert!(
            !summary.contains("secret-stdin-body"),
            "stdin body leaked: {summary}"
        );
    }

    #[test]
    fn test_summarize_tool_args_empty_is_none() {
        assert!(summarize_tool_args("read_file", &serde_json::json!({})).is_none());
        assert!(summarize_tool_args("read_file", &serde_json::Value::Null).is_none());
    }

    // ---- L1: disabled/invalid activity row drops events (no silent fallback) ----

    #[test]
    fn test_webhook_tool_event_sink_disabled_activity_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_activity_row(&conn, "https://discord.com/api/webhooks/1/tok", false);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({});
        opencrab_actions::ToolEventSink::on_event(&sink, &started_event(&args));
        assert!(rx.try_recv().is_err(), "disabled activity -> nothing sent");
    }

    #[test]
    fn test_webhook_tool_event_sink_invalid_activity_sends_nothing() {
        let conn = opencrab_db::init_memory().unwrap();
        // invalid (non-discord) url -> WebhookResolution::Error, must drop, no fallback.
        insert_activity_row(&conn, "https://evil.example.com/api/webhooks/1/tok", true);
        let db = opencrab_db::Db::from_connection(conn);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DeliveryBatch>();
        let sink = make_sink(db, tx);
        let args = serde_json::json!({});
        opencrab_actions::ToolEventSink::on_event(&sink, &started_event(&args));
        assert!(
            rx.try_recv().is_err(),
            "invalid activity url -> nothing sent"
        );
    }

    // ---- L2: shared delivery path preserves lifecycle/tool_call ordering ----

