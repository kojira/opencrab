    use super::*;
    use crate::engine::{ChatRequest, ChatResponse, LlmClient};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct MockLlm;

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
            ))
        }
    }

    struct RecordingMockLlm {
        last_request: Arc<Mutex<Option<ChatRequest>>>,
    }

    #[async_trait]
    impl LlmClient for RecordingMockLlm {
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
            *self.last_request.lock().unwrap() = Some(req);
            Ok(ChatResponse::text(
                r#"{"title": "テストトピック", "summary": "テスト要約です。"}"#.to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_build_incremental_empty() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(result.nodes_created, 0);
        assert_eq!(result.logs_indexed, 0);
    }

    #[tokio::test]
    async fn test_build_incremental_with_logs() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // Insert some test logs
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello, this is a test message about Rust programming.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let log2 = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Yes, Rust is great for systems programming.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(2),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log2).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();

        // root + period + session + topic = 4 nodes
        assert_eq!(result.nodes_created, 4);
        assert_eq!(result.logs_indexed, 2);

        // Verify watermark
        let db = conn.lock().unwrap();
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(wm.last_indexed_log_id, 2);
        assert_eq!(wm.total_nodes, 4);

        // Verify tree structure
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert_eq!(tree.len(), 4);
        assert!(tree.iter().any(|n| n.node_type == "root"));
        assert!(tree.iter().any(|n| n.node_type == "period"));
        assert!(tree.iter().any(|n| n.node_type == "session"));
        assert!(tree.iter().any(|n| n.node_type == "topic"));

        // Topic node should have LLM-generated title
        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        assert_eq!(topic.title, "テストトピック");
        assert_eq!(topic.summary, "テスト要約です。");
        // keywords 無しの旧形式応答 → title フォールバック（空にはしない）
        assert_eq!(topic.keywords_json, r#"["テストトピック"]"#);
    }

    struct KeywordMockLlm;

    #[async_trait]
    impl LlmClient for KeywordMockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                r#"{"title": "Rust勉強会", "summary": "所有権を学んだ。", "keywords": ["Rust", "所有権", " Rust ", ""]}"#
                    .to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn test_topic_keywords_from_llm_normalized_and_searchable() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Rust ownership discussion".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        IndexBuilder::build_incremental(&conn, "agent-1", &KeywordMockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        // トリム・空要素除去・重複除去される
        assert_eq!(topic.keywords_json, r#"["Rust","所有権"]"#);
        // FTS 逆引きでキーワードから引ける
        let hits =
            opencrab_db::queries::search_index_nodes(&db, "agent-1", "所有権", 10, None).unwrap();
        assert!(hits.iter().any(|h| h.node_id == topic.id));
    }

    #[tokio::test]
    async fn test_merge_topics_leaves_no_fts_orphans() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 同一月に 3 セッション分のログ → topic 3 個
        for s in 1..=3 {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: format!("session-{s}"),
                log_type: "message".to_string(),
                content: format!("unique-marker-{s} content"),
                speaker_id: Some("user-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();
        }
        let conn = opencrab_db::Db::from_connection(db_conn);
        IndexBuilder::build_incremental(&conn, "agent-1", &KeywordMockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let merge = IndexBuilder::merge_topics(&conn, "agent-1", &KeywordMockLlm, "m", 1, "", None)
            .await
            .unwrap();
        assert!(merge.topics_deleted >= 2);

        let db = conn.lock().unwrap();
        // FTS 行数 = ノード行数（孤児なし）
        let fts: i64 = db
            .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
            .unwrap();
        let nodes: i64 = db
            .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, nodes);
        // マージノードは元トピックの keywords を引き継ぐ
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let merged = tree
            .iter()
            .find(|n| n.id.starts_with("merged-topic-"))
            .unwrap();
        assert!(merged.keywords_json.contains("Rust"));
    }

    /// T-2.1: ペルソナ情報が要約プロンプトに含まれる
    #[tokio::test]
    async fn test_persona_prompt_contains_persona_info() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello, this is a test message.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let _result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "エージェントC",
            Some("17歳のオタク高校生"),
        )
        .await
        .unwrap();

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("");
        assert!(
            prompt.contains("エージェントC"),
            "プロンプトにペルソナ名が含まれるべき"
        );
        assert!(
            prompt.contains("17歳のオタク高校生"),
            "プロンプトにpersonalityが含まれるべき"
        );
    }

    /// T-2.2: 注目ポイント4軸がプロンプトに含まれる
    #[tokio::test]
    async fn test_persona_prompt_contains_four_axes() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message for four axes check.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let _result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &llm,
            "test-model",
            50,
            "テスト",
            Some("テスト用ペルソナ"),
        )
        .await
        .unwrap();

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("");
        assert!(
            prompt.contains("学んだこと") || prompt.contains("技術知見"),
            "技術知見軸が含まれるべき"
        );
        assert!(
            prompt.contains("判断の理由") || prompt.contains("判断"),
            "判断軸が含まれるべき"
        );
        assert!(
            prompt.contains("関係性") || prompt.contains("感情"),
            "関係性・感情軸が含まれるべき"
        );
        assert!(
            prompt.contains("失敗") || prompt.contains("教訓"),
            "失敗・教訓軸が含まれるべき"
        );
    }

    /// T-2.5: ペルソナが空でもエラーにならずデフォルト一人称で要約される
    #[tokio::test]
    async fn test_persona_empty_uses_default() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message for empty persona.".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let last_request = Arc::new(Mutex::new(None));
        let llm = RecordingMockLlm {
            last_request: last_request.clone(),
        };

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();

        assert!(result.nodes_created > 0, "ノードが生成されるべき");

        let request = last_request.lock().unwrap().clone().unwrap();
        let prompt = request.messages[1].text_content().unwrap_or("");
        // Default prompt should still use 一人称
        assert!(
            prompt.contains("一人称"),
            "デフォルトプロンプトに一人称が含まれるべき"
        );
    }

    #[tokio::test]
    async fn test_build_incremental_idempotent() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Test message".to_string(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // First build
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert!(r1.nodes_created > 0);

        // Second build should create no new nodes (no new logs)
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r2.nodes_created, 0);
        assert_eq!(r2.logs_indexed, 0);
    }

    /// LLM 呼び出しそのものが失敗するモック（#378）。
    struct FailingLlm;

    #[async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Err(anyhow::anyhow!("simulated provider failure"))
        }
    }

    /// JSON ではない応答を返すモック（要約経路の `Ok` だがパース失敗ケース）。
    struct InvalidJsonMockLlm;

    #[async_trait]
    impl LlmClient for InvalidJsonMockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(
                "これはJSONではない普通の文章です".to_string(),
            ))
        }
    }

    /// #378: LLM 呼び出しが Err のときはプレースホルダ topic を作らずスキップする。
    /// ただし watermark は前進させ、同じログを毎 tick 取り直す無限ループ（#374 の罠）を防ぐ。
    #[tokio::test]
    async fn test_llm_error_skips_topic_but_advances_watermark() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 2);
        let conn = opencrab_db::Db::from_connection(db_conn);

        let result = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &FailingLlm,
            "test-model",
            50,
            "",
            None,
        )
        .await
        .unwrap();

        // logs は取得されている（スキップされたのは topic 生成だけ）
        assert_eq!(result.logs_indexed, 2);

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        // topic ノードは 1 件も作られない（"Summary generation failed" プレースホルダを作らない）
        assert!(
            !tree.iter().any(|n| n.node_type == "topic"),
            "LLM エラー時に topic を作ってはならない"
        );
        assert!(
            !tree
                .iter()
                .any(|n| n.summary == "Summary generation failed"),
            "'Summary generation failed' プレースホルダを作ってはならない"
        );

        // watermark は最終ログ ID まで前進している（再ビルドで再取得しない）
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            wm.last_indexed_log_id, 2,
            "watermark は失敗レンジを追い越して前進すべき"
        );
        drop(db);

        // 再ビルドしても新しいログは取得されない（毎 tick 再取得ループにならない）
        let r2 = IndexBuilder::build_incremental(
            &conn,
            "agent-1",
            &FailingLlm,
            "test-model",
            50,
            "",
            None,
        )
        .await
        .unwrap();
        assert_eq!(r2.logs_indexed, 0, "watermark 前進済みなので再取得しない");
    }

    /// #378: JSON パース失敗（応答は返っている）ケースは従来どおり topic を作る。
    /// 先頭ログ本文の先頭 100 字を summary に使う既存挙動を変えない。
    #[tokio::test]
    async fn test_invalid_json_still_creates_topic() {
        let db_conn = opencrab_db::init_memory().unwrap();
        let content = "Rust の所有権について議論した内容のログ".to_string();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: content.clone(),
            speaker_id: Some("user-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&db_conn, &log).unwrap();
        let conn = opencrab_db::Db::from_connection(db_conn);

        IndexBuilder::build_incremental(&conn, "agent-1", &InvalidJsonMockLlm, "m", 50, "", None)
            .await
            .unwrap();

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topic = tree
            .iter()
            .find(|n| n.node_type == "topic")
            .expect("パース失敗時も topic は作られるべき");
        // 100 字未満なので先頭ログ本文がそのまま summary になる
        assert_eq!(topic.summary, content);
        assert_ne!(topic.summary, "Summary generation failed");
        assert_eq!(topic.title, "Topic (logs 1-1)");
    }

    /// ヘルパー: 指定セッションにN件のログを投入
    fn insert_logs(conn: &rusqlite::Connection, agent_id: &str, session_id: &str, count: usize) {
        for i in 0..count {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "message".to_string(),
                content: format!("Message {i} in session {session_id}"),
                speaker_id: Some(if i % 2 == 0 {
                    "user-1".to_string()
                } else {
                    agent_id.to_string()
                }),
                turn_number: Some(i as i32),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(conn, &log).unwrap();
        }
    }

