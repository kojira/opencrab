    /// ヘルパー: 指定セッションにログを created_at 付きで投入
    /// insert_session_log は常に Utc::now() を使うため、INSERT 後に UPDATE で上書きする
    fn insert_log_with_date(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        turn: i32,
        content: &str,
        created_at: &str,
    ) {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "message".to_string(),
            content: content.to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(turn),
            metadata_json: None,
            created_at: Some(created_at.to_string()),
        };
        let row_id = opencrab_db::queries::insert_session_log(conn, &log).unwrap();
        conn.execute(
            "UPDATE memory_sessions SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![created_at, row_id],
        )
        .unwrap();
    }

    /// T-1.10: 同一日のログ → date_from == date_to == その日
    #[tokio::test]
    async fn test_build_date_same_day() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            insert_log_with_date(
                &c,
                "agent-d",
                "sess-d",
                0,
                "Morning msg",
                "2026-04-01 09:00:00",
            );
            insert_log_with_date(
                &c,
                "agent-d",
                "sess-d",
                1,
                "Afternoon msg",
                "2026-04-01 15:00:00",
            );
        }

        let result =
            IndexBuilder::build_incremental(&conn, "agent-d", &llm, "test-model", 2, "", None)
                .await
                .unwrap();
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-d", "sess-d").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        assert_eq!(t.date_from.as_deref(), Some("2026-04-01"));
        assert_eq!(t.date_to.as_deref(), Some("2026-04-01"));
    }

    /// T-1.11: 複数日にまたがるログ → date_from < date_to
    #[tokio::test]
    async fn test_build_date_multi_day() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            insert_log_with_date(
                &c,
                "agent-m",
                "sess-m",
                0,
                "Day 1 msg",
                "2026-04-01 10:00:00",
            );
            insert_log_with_date(
                &c,
                "agent-m",
                "sess-m",
                1,
                "Day 3 msg",
                "2026-04-03 14:00:00",
            );
        }

        let result =
            IndexBuilder::build_incremental(&conn, "agent-m", &llm, "test-model", 2, "", None)
                .await
                .unwrap();
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-m", "sess-m").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        assert_eq!(t.date_from.as_deref(), Some("2026-04-01"));
        assert_eq!(t.date_to.as_deref(), Some("2026-04-03"));
    }

    /// T-1.12: created_at が短い/空文字のログ → date_from/date_to は None（パニックしない）
    /// memory_sessions.created_at は NOT NULL なので NULL にはできないが、
    /// 空文字や短い文字列でも s[..10] スライスでパニックしないことを検証する
    #[tokio::test]
    async fn test_build_date_null_created_at() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        {
            let c = conn.lock().unwrap();
            // Use the existing insert_logs helper which sets created_at to now()
            insert_logs(&c, "agent-n", "sess-n", 3);
            // Set created_at to empty string to simulate missing/invalid date
            c.execute_batch(
                "UPDATE memory_sessions SET created_at = '' WHERE agent_id = 'agent-n'",
            )
            .unwrap();
        }

        let result =
            IndexBuilder::build_incremental(&conn, "agent-n", &llm, "test-model", 3, "", None)
                .await
                .unwrap();
        // Should not panic, nodes should still be created
        assert!(result.nodes_created > 0);

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-n", "sess-n").unwrap();
        assert!(!topics.is_empty());
        let t = &topics[0];
        // date_from/date_to should be None when all logs have empty created_at
        assert_eq!(t.date_from, None);
        assert_eq!(t.date_to, None);
    }


    /// D-1078: a long-lived session's later topics go under the month they happened in,
    /// not under the month the session started. Each month gets its own session node.
    #[tokio::test]
    async fn long_session_topics_go_under_their_own_month() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;
        let batches = [
            ("2025-08-20 10:00:00", "2025-08-20 10:05:00"),
            ("2025-09-03 10:00:00", "2025-09-03 10:05:00"),
            ("2025-10-07 10:00:00", "2025-10-07 10:05:00"),
        ];
        for (i, (a, b)) in batches.iter().enumerate() {
            {
                let c = conn.lock().unwrap();
                insert_log_with_date(&c, "agent-l", "sess-l", 0, &format!("msg {i} a"), a);
                insert_log_with_date(&c, "agent-l", "sess-l", 1, &format!("msg {i} b"), b);
            }
            IndexBuilder::build_incremental(&conn, "agent-l", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        }

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-l", "sess-l").unwrap();
        assert_eq!(topics.len(), 3);
        for t in &topics {
            let month = &t.date_from.as_deref().unwrap()[..7];
            let session = opencrab_db::queries::get_index_node(&c, t.parent_id.as_deref().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(session.id, format!("session-agent-l-sess-l-{month}"));
            assert_eq!(session.source_session_id.as_deref(), Some("sess-l"));
            assert_eq!(
                session.parent_id.as_deref(),
                Some(format!("period-agent-l-{month}").as_str()),
                "topic {month} must sit under its own month"
            );
        }
        let counts = opencrab_db::queries::count_topics_per_period(&c, "agent-l").unwrap();
        assert_eq!(counts.get("period-agent-l-2025-08"), Some(&1));
        assert_eq!(counts.get("period-agent-l-2025-09"), Some(&1));
        assert_eq!(counts.get("period-agent-l-2025-10"), Some(&1));
    }

    /// D-1078: a rolled-up past month is not made stale again when the same live session
    /// adds topics in a later month.
    #[tokio::test]
    async fn rolled_past_month_is_not_rerolled_by_later_topics_of_same_session() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;
        {
            let c = conn.lock().unwrap();
            insert_log_with_date(&c, "agent-r", "sess-r", 0, "aug a", "2025-08-20 10:00:00");
            insert_log_with_date(&c, "agent-r", "sess-r", 1, "aug b", "2025-08-20 10:05:00");
        }
        IndexBuilder::build_incremental(&conn, "agent-r", &llm, "test-model", 10, "", None)
            .await
            .unwrap();
        {
            let c = conn.lock().unwrap();
            let stale = opencrab_db::queries::find_stale_period(&c, "agent-r")
                .unwrap()
                .unwrap();
            assert_eq!(stale.title, "2025-08");
            opencrab_db::queries::update_period_rollup(&c, &stale.id, "Aug summary", "[]")
                .unwrap();
            assert!(opencrab_db::queries::find_stale_period(&c, "agent-r")
                .unwrap()
                .is_none());
            insert_log_with_date(&c, "agent-r", "sess-r", 2, "sep a", "2025-09-02 10:00:00");
            insert_log_with_date(&c, "agent-r", "sess-r", 3, "sep b", "2025-09-02 10:05:00");
        }
        IndexBuilder::build_incremental(&conn, "agent-r", &llm, "test-model", 10, "", None)
            .await
            .unwrap();

        let c = conn.lock().unwrap();
        let stale = opencrab_db::queries::find_stale_period(&c, "agent-r")
            .unwrap()
            .unwrap();
        assert_eq!(stale.title, "2025-09", "only the new month needs a rollup");
        opencrab_db::queries::update_period_rollup(&c, &stale.id, "Sep summary", "[]").unwrap();
        assert!(
            opencrab_db::queries::find_stale_period(&c, "agent-r")
                .unwrap()
                .is_none(),
            "2025-08 must stay rolled"
        );
        let aug = opencrab_db::queries::get_index_node(&c, "period-agent-r-2025-08")
            .unwrap()
            .unwrap();
        assert_eq!(aug.summary, "Aug summary");
    }

    /// D-1078 compatibility: a pre-existing (un-suffixed) session node keeps receiving the
    /// topics of its own month, so old data does not get a duplicate session node.
    #[tokio::test]
    async fn legacy_session_node_is_reused_for_its_own_month_only() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;
        {
            let c = conn.lock().unwrap();
            insert_log_with_date(&c, "agent-g", "sess-g", 0, "aug a", "2025-08-20 10:00:00");
            insert_log_with_date(&c, "agent-g", "sess-g", 1, "aug b", "2025-08-20 10:05:00");
        }
        IndexBuilder::build_incremental(&conn, "agent-g", &llm, "test-model", 10, "", None)
            .await
            .unwrap();
        {
            // Rewrite the month node into the pre-D-1078 id shape.
            let c = conn.lock().unwrap();
            c.execute_batch(
                "PRAGMA foreign_keys = OFF;
                 UPDATE memory_index_nodes SET id = 'session-agent-g-sess-g'
                   WHERE id = 'session-agent-g-sess-g-2025-08';
                 UPDATE memory_index_nodes SET parent_id = 'session-agent-g-sess-g'
                   WHERE parent_id = 'session-agent-g-sess-g-2025-08';
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            insert_log_with_date(&c, "agent-g", "sess-g", 2, "aug c", "2025-08-25 10:00:00");
            insert_log_with_date(&c, "agent-g", "sess-g", 3, "sep a", "2025-09-02 10:00:00");
        }
        // batch_size 1: one topic per log so the two months stay separate.
        IndexBuilder::build_incremental(&conn, "agent-g", &llm, "test-model", 1, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-g", &llm, "test-model", 1, "", None)
            .await
            .unwrap();

        let c = conn.lock().unwrap();
        let topics =
            opencrab_db::queries::get_topic_nodes_for_session(&c, "agent-g", "sess-g").unwrap();
        let parents: Vec<(String, String)> = topics
            .iter()
            .map(|t| {
                (
                    t.date_from.clone().unwrap(),
                    t.parent_id.clone().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            parents,
            vec![
                ("2025-08-20".into(), "session-agent-g-sess-g".into()),
                ("2025-08-25".into(), "session-agent-g-sess-g".into()),
                ("2025-09-02".into(), "session-agent-g-sess-g-2025-09".into()),
            ]
        );
    }
