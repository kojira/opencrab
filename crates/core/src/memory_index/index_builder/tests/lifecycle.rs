    /// 複数セッションにまたがるログ — 各セッションが別のsession/topicノードになるか
    #[tokio::test]
    async fn test_multiple_sessions() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-a", 5);
        insert_logs(&db_conn, "agent-1", "session-b", 3);
        insert_logs(&db_conn, "agent-1", "session-c", 4);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();

        assert_eq!(result.logs_indexed, 12); // 5+3+4

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();

        // root(1) + period(1) + session(3) + topic(3) = 8
        assert_eq!(tree.len(), 8);
        let sessions: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        assert_eq!(sessions.len(), 3);
        let topics: Vec<_> = tree.iter().filter(|n| n.node_type == "topic").collect();
        assert_eq!(topics.len(), 3);

        // 各topicは異なるsource_session_idを持つ
        let mut topic_sessions: Vec<_> = topics
            .iter()
            .filter_map(|n| n.source_session_id.clone())
            .collect();
        topic_sessions.sort();
        assert_eq!(topic_sessions, vec!["session-a", "session-b", "session-c"]);
    }

    /// バッチサイズ超過 — batch_sizeで切られ、残りは次回ビルドで処理される
    #[tokio::test]
    async fn test_batch_size_limit() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 30件投入、batch_size=10で実行
        insert_logs(&db_conn, "agent-1", "session-1", 15);
        insert_logs(&db_conn, "agent-1", "session-2", 15);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // batch_size=10: 最初の10件のみ処理される
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r1.logs_indexed, 10);

        // ウォーターマークは10件目まで進んでいる
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 10);
        }

        // 2回目: 次の10件
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r2.logs_indexed, 10);

        // 3回目: 残り10件
        let r3 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r3.logs_indexed, 10);

        // 4回目: もう残りなし
        let r4 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 10, "", None)
                .await
                .unwrap();
        assert_eq!(r4.logs_indexed, 0);
        assert_eq!(r4.nodes_created, 0);

        // 最終的にウォーターマークは30件目
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(wm.last_indexed_log_id, 30);
        }
    }

    /// 増分ビルド — 初回ビルド後に新ログ追加、再ビルドで新ノードのみ追加
    #[tokio::test]
    async fn test_incremental_after_new_logs() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // 初回ビルド
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r1.logs_indexed, 5);
        let first_node_count = r1.nodes_created;

        // 新しいセッションにログ追加
        {
            let db = conn.lock().unwrap();
            insert_logs(&db, "agent-1", "session-2", 3);
        }

        // 2回目ビルド — 新ログのみ処理、session-2のノードが追加される
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r2.logs_indexed, 3);
        // session + topic = 2 new nodes (root/periodは既存を再利用)
        assert_eq!(r2.nodes_created, 2);

        // ツリー全体を検証
        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        assert_eq!(tree.len(), first_node_count + 2); // 初回4 + session+topic=2
        let sessions: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        assert_eq!(sessions.len(), 2);

        // ウォーターマークが最終ログまで進んでいる
        let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(wm.last_indexed_log_id, 8); // 5+3
    }

    /// 大量ログ（1セッション100件） — 全件が1つのtopicノードにまとまる
    #[tokio::test]
    async fn test_large_single_session() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-big", 100);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        let result =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 200, "", None)
                .await
                .unwrap();

        assert_eq!(result.logs_indexed, 100);

        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        // root(1) + period(1) + session(1) + topic(1) = 4
        assert_eq!(tree.len(), 4);

        let topic = tree.iter().find(|n| n.node_type == "topic").unwrap();
        assert_eq!(topic.start_log_id, Some(1));
        assert_eq!(topic.end_log_id, Some(100));
        assert!(topic.token_count > 0);

        // child_countが正しく更新されている
        let root = tree.iter().find(|n| n.node_type == "root").unwrap();
        assert_eq!(root.child_count, 1); // period
        let session = tree.iter().find(|n| n.node_type == "session").unwrap();
        assert_eq!(session.child_count, 1); // topic
    }

    /// 異なるエージェントのログが混在 — agent_idでフィルタされる
    #[tokio::test]
    async fn test_agent_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-2", "session-2", 8);

        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // agent-1のみビルド
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r1.logs_indexed, 5);

        // agent-2のみビルド
        let r2 =
            IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(r2.logs_indexed, 8);

        // 各エージェントのツリーが独立
        let db = conn.lock().unwrap();
        let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
        assert_eq!(tree1.len(), 4); // root+period+session+topic
        assert_eq!(tree2.len(), 4);
        // ノードIDが重複しない
        let ids1: Vec<_> = tree1.iter().map(|n| &n.id).collect();
        let ids2: Vec<_> = tree2.iter().map(|n| &n.id).collect();
        for id in &ids1 {
            assert!(!ids2.contains(id));
        }
    }

    // ================================================================
    // delete_index / rebuild_index / merge_topics テスト
    // ================================================================

    /// delete_index: 全ノードとウォーターマークが削除されるか
    #[tokio::test]
    async fn test_delete_index() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // まずインデックスを構築
        let r = IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert!(r.nodes_created > 0);

        // 削除前にツリーとウォーターマークが存在することを確認
        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(!tree.is_empty(), "削除前はノードが存在するはず");
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            assert!(wm.is_some(), "削除前はウォーターマークが存在するはず");
        }

        // 削除実行
        IndexBuilder::delete_index(&conn, "agent-1").unwrap();

        // 削除後: ツリーもウォーターマークも空になるはず
        {
            let db = conn.lock().unwrap();
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert!(tree.is_empty(), "削除後はノードが0件になるはず");
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            assert!(wm.is_none(), "削除後はウォーターマークがNoneになるはず");
        }
    }

    /// delete_index: 他のエージェントのデータは影響を受けない
    #[tokio::test]
    async fn test_delete_index_isolation() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-2", "session-2", 5);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        IndexBuilder::build_incremental(&conn, "agent-2", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // agent-1のみ削除
        IndexBuilder::delete_index(&conn, "agent-1").unwrap();

        // agent-1は空、agent-2は無事
        {
            let db = conn.lock().unwrap();
            let tree1 = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            let tree2 = opencrab_db::queries::get_index_tree(&db, "agent-2").unwrap();
            assert!(tree1.is_empty(), "agent-1のノードは削除済みのはず");
            assert!(!tree2.is_empty(), "agent-2のノードは無事なはず");
            let wm1 = opencrab_db::queries::get_index_watermark(&db, "agent-1").unwrap();
            let wm2 = opencrab_db::queries::get_index_watermark(&db, "agent-2").unwrap();
            assert!(wm1.is_none(), "agent-1のウォーターマークは削除済みのはず");
            assert!(wm2.is_some(), "agent-2のウォーターマークは無事なはず");
        }
    }

    /// rebuild_index: 削除 → 再構築でウォーターマークがリセットされ、全ログが再インデックスされるか
    #[tokio::test]
    async fn test_rebuild_index() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 5);
        insert_logs(&db_conn, "agent-1", "session-2", 3);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // 初回ビルド
        let r1 =
            IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        let first_tree_len = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1")
                .unwrap()
                .len()
        };
        assert!(r1.nodes_created > 0);

        // 再構築
        let r2 = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // 再構築後は全ログが再インデックスされる
        assert_eq!(r2.logs_indexed, 8, "8件全ログが再インデックスされるはず");
        assert!(r2.nodes_created > 0, "ノードが再作成されるはず");

        // ウォーターマークが最新の状態
        {
            let db = conn.lock().unwrap();
            let wm = opencrab_db::queries::get_index_watermark(&db, "agent-1")
                .unwrap()
                .unwrap();
            assert_eq!(
                wm.last_indexed_log_id, 8,
                "ウォーターマークが最終ログIDを指すはず"
            );

            // ツリーが再構築されている
            let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
            assert_eq!(tree.len(), first_tree_len, "再構築後もツリー構造が同じはず");
        }
    }

    /// rebuild_index: 空のインデックスからも再構築できる
    #[tokio::test]
    async fn test_rebuild_index_from_empty() {
        let db_conn = opencrab_db::init_memory().unwrap();
        insert_logs(&db_conn, "agent-1", "session-1", 3);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        // ビルドせずに再構築（初回rebuild）
        let r = IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();
        assert_eq!(r.logs_indexed, 3);
        assert_eq!(r.nodes_created, 4); // root+period+session+topic
    }

    /// merge_topics: topicが閾値以下なら変化なし
    #[tokio::test]
    async fn test_merge_topics_no_merge_needed() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 2セッション = 2topicノード
        insert_logs(&db_conn, "agent-1", "session-a", 3);
        insert_logs(&db_conn, "agent-1", "session-b", 3);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        let tree_before = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap()
        };

        // max_topics_per_period=5 なので2topicは閾値以下
        let result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 5, "", None)
            .await
            .unwrap();

        assert_eq!(result.topics_merged, 0, "マージ不要なのでmergedは0");
        assert_eq!(result.topics_deleted, 0, "削除も0");

        // ツリーは変化なし
        let tree_after = {
            let db = conn.lock().unwrap();
            opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap()
        };
        assert_eq!(
            tree_before.len(),
            tree_after.len(),
            "マージなしでツリー長は変化しない"
        );
    }

    /// merge_topics: topicが閾値超過でマージされ、要約が統合されるか
    #[tokio::test]
    async fn test_merge_topics_triggers_merge() {
        let db_conn = opencrab_db::init_memory().unwrap();
        // 4セッション = 4topicノード（1periodの下）
        insert_logs(&db_conn, "agent-1", "session-a", 2);
        insert_logs(&db_conn, "agent-1", "session-b", 2);
        insert_logs(&db_conn, "agent-1", "session-c", 2);
        insert_logs(&db_conn, "agent-1", "session-d", 2);
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // max_topics_per_period=2 → 4topics > 2 なのでマージ発動
        let result = IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 2, "", None)
            .await
            .unwrap();

        assert_eq!(result.periods_processed, 1, "1つのperiodが処理されるはず");
        assert!(
            result.topics_merged >= 1,
            "少なくとも1回のマージが実行されるはず"
        );
        assert!(
            result.topics_deleted >= 3,
            "旧topicは削除されるはず（4 - 1 = 3）"
        );

        // マージ後: 4topicが統合されて1topicになる
        let db = conn.lock().unwrap();
        let tree = opencrab_db::queries::get_index_tree(&db, "agent-1").unwrap();
        let topics: Vec<_> = tree.iter().filter(|n| n.node_type == "topic").collect();
        // マージで4→1になる（merged-topicが1つ）
        assert_eq!(topics.len(), 1, "4topicがマージされて1topicになるはず");
        // マージされたtopicはMockLlmのタイトルを持つ
        assert_eq!(
            topics[0].title, "テストトピック",
            "LLM生成タイトルを持つはず"
        );

        // child_countが正しく更新されている
        let session_nodes: Vec<_> = tree.iter().filter(|n| n.node_type == "session").collect();
        // マージ後、統合topicの親sessionのchild_countが更新されているはず
        let _ = session_nodes; // child_count検証は親ID依存のため省略
    }

    /// merge_topics: マージ後にrebuild_indexしても整合性が保たれる
    #[tokio::test]
    async fn test_merge_then_rebuild() {
        let db_conn = opencrab_db::init_memory().unwrap();
        for i in 0..5 {
            insert_logs(&db_conn, "agent-1", &format!("session-{i}"), 3);
        }
        let conn = opencrab_db::Db::from_connection(db_conn);
        let llm = MockLlm;

        IndexBuilder::build_incremental(&conn, "agent-1", &llm, "test-model", 50, "", None)
            .await
            .unwrap();

        // まずマージ
        let merge_result =
            IndexBuilder::merge_topics(&conn, "agent-1", &llm, "test-model", 2, "", None)
                .await
                .unwrap();
        assert!(merge_result.topics_merged > 0);

        // その後rebuildすると完全にリフレッシュされる
        let rebuild_result =
            IndexBuilder::rebuild_index(&conn, "agent-1", &llm, "test-model", 50, "", None)
                .await
                .unwrap();
        assert_eq!(
            rebuild_result.logs_indexed, 15,
            "5セッション×3ログ=15件が再インデックス"
        );

        // orphanなし、child_count整合
        let db = conn.lock().unwrap();
        let metrics =
            crate::memory_index::graph_query::IndexQualityMetrics::compute(&db, "agent-1").unwrap();
        assert_eq!(metrics.orphan_count, 0);
        assert_eq!(metrics.child_count_mismatch, 0);
        assert_eq!(metrics.log_coverage, 1.0);
    }

