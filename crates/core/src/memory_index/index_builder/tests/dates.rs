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

