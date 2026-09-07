    use super::*;
    use crate::traits::{ActionContext, CallerIdentity};
    use serde_json::json;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    fn seed_logs(ctx: &ActionContext, n: usize) {
        let conn = ctx.db.lock().unwrap();
        for i in 0..n {
            opencrab_db::queries::insert_session_log(
                &conn,
                &opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: "agent-1".to_string(),
                    session_id: "session-1".to_string(),
                    log_type: "message".to_string(),
                    content: format!("発話 {i}"),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                },
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn survey_empty_history() {
        let (_d, ctx) = test_context();
        let r = SurveyMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(r.success);
        let data = r.data.unwrap();
        assert_eq!(data["total_logs"], 0);
        assert_eq!(data["granularity"], "day");
    }

    #[tokio::test]
    async fn read_requires_a_range() {
        let (_d, ctx) = test_context();
        let r = ReadMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn read_rejects_multiple_ranges() {
        let (_d, ctx) = test_context();
        let r = ReadMyHistoryAction
            .execute(&json!({"session_id": "s", "around_id": 1}), &ctx)
            .await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn record_read_and_retract_roundtrip() {
        let (_d, ctx) = test_context();
        seed_logs(&ctx, 4);

        // record
        let r = RecordMemoryUnitAction
            .execute(
                &json!({"from_id": 1, "to_id": 4, "title": "所有権の話", "tags": ["Rust"]}),
                &ctx,
            )
            .await;
        assert!(r.success, "record failed: {:?}", r.error);
        let data = r.data.unwrap();
        let short_id = data["short_id"].as_str().unwrap().to_string();
        assert_eq!(data["logs_in_range"], 4);
        assert_eq!(data["tags"][0], "Rust");
        assert!(data["tag_error"].is_null());

        // read (id range)
        let rr = ReadMyHistoryAction
            .execute(&json!({"from_id": 1, "to_id": 4}), &ctx)
            .await;
        assert!(rr.success);
        assert_eq!(rr.data.unwrap()["returned"], 4);

        // retract by short_id
        let rt = RetractMemoryUnitAction
            .execute(&json!({"unit_id": short_id}), &ctx)
            .await;
        assert!(rt.success, "retract failed: {:?}", rt.error);
        assert_eq!(rt.data.unwrap()["retracted"], true);
    }

    #[tokio::test]
    async fn record_rejects_empty_range() {
        let (_d, ctx) = test_context();
        // 生ログが無いので範囲は空 → エラー。
        let r = RecordMemoryUnitAction
            .execute(&json!({"from_id": 1, "to_id": 4, "title": "x"}), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("生ログ"));
    }

    #[tokio::test]
    async fn retract_rejects_missing_unit() {
        let (_d, ctx) = test_context();
        let r = RetractMemoryUnitAction
            .execute(&json!({"unit_id": "nope"}), &ctx)
            .await;
        assert!(!r.success);
    }

    // ---- 記憶の凝縮（core / #411）----

    /// 2 つのユニットを宣言し、その short_id を返すヘルパ。
    async fn seed_two_units(ctx: &ActionContext) -> (String, String) {
        seed_logs(ctx, 4);
        let a = RecordMemoryUnitAction
            .execute(
                &json!({"from_id": 1, "to_id": 2, "title": "沈黙を選んだ夜"}),
                ctx,
            )
            .await;
        let b = RecordMemoryUnitAction
            .execute(
                &json!({"from_id": 3, "to_id": 4, "title": "差分だけ拾うと決めた朝"}),
                ctx,
            )
            .await;
        (
            a.data.unwrap()["short_id"].as_str().unwrap().to_string(),
            b.data.unwrap()["short_id"].as_str().unwrap().to_string(),
        )
    }

    #[tokio::test]
    async fn core_record_update_retract_roundtrip_and_source_links() {
        let (_d, ctx) = test_context();
        let (u1, u2) = seed_two_units(&ctx).await;

        // record: 2 つのユニットを根拠に原則を刻む。
        let r = RecordMemoryCoreAction
            .execute(
                &json!({
                    "axis": "繰り返していること",
                    "body": "静けさのなかで差分だけを拾おうとし続けている",
                    "sources": [u1, u2],
                }),
                &ctx,
            )
            .await;
        assert!(r.success, "record_memory_core failed: {:?}", r.error);
        let data = r.data.unwrap();
        let core_id = data["short_id"].as_str().unwrap().to_string();
        // 根拠リンクが echo され、id 範囲が元ユニットの min/max に畳まれる。
        assert_eq!(data["sources"].as_array().unwrap().len(), 2);
        assert!(data["unresolved_sources"].as_array().unwrap().is_empty());
        assert_eq!(data["start_log_id"], 1, "id 範囲の下端は元ユニットの min");
        assert_eq!(data["end_log_id"], 4, "id 範囲の上端は元ユニットの max");

        // 格納は node_type='meta'（人格の核）。
        {
            let conn = ctx.db.lock().unwrap();
            let cores = opencrab_db::queries::list_memory_cores(&conn, "agent-1").unwrap();
            assert_eq!(cores.len(), 1);
            assert_eq!(cores[0].node_type, "meta");
            assert_eq!(cores[0].source_type, "condensed");
            // keywords_json に根拠 short_id の配列が入る。
            let srcs: Vec<String> = serde_json::from_str(&cores[0].keywords_json).unwrap();
            assert!(srcs.contains(&"u1".to_string()) || srcs.len() == 2);
        }

        // update: 本文だけ書き直す（sources 省略で根拠維持）。
        let up = UpdateMemoryCoreAction
            .execute(
                &json!({"core_id": core_id, "axis": "繰り返していること", "body": "書き直した本文"}),
                &ctx,
            )
            .await;
        assert!(up.success, "update failed: {:?}", up.error);
        assert_eq!(
            up.data.unwrap()["sources"].as_array().unwrap().len(),
            2,
            "根拠は維持される"
        );

        // retract。
        let rt = RetractMemoryCoreAction
            .execute(&json!({"core_id": core_id}), &ctx)
            .await;
        assert!(rt.success, "retract failed: {:?}", rt.error);
        {
            let conn = ctx.db.lock().unwrap();
            assert!(opencrab_db::queries::list_memory_cores(&conn, "agent-1")
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn core_record_rejects_when_no_source_resolves() {
        let (_d, ctx) = test_context();
        seed_two_units(&ctx).await;
        // 存在しない short_id だけ → 根拠 0 件で拒否（平均化を防ぐ / #411 原則3）。
        let r = RecordMemoryCoreAction
            .execute(
                &json!({"axis": "x", "body": "y", "sources": ["nope1", "nope2"]}),
                &ctx,
            )
            .await;
        assert!(!r.success);
    }

    #[tokio::test]
    async fn core_tools_reject_non_meta_node() {
        let (_d, ctx) = test_context();
        let (u1, _u2) = seed_two_units(&ctx).await;
        // ユニット（node_type='unit'）を core として更新/取消しようとしても弾かれる。
        let up = UpdateMemoryCoreAction
            .execute(&json!({"core_id": u1, "axis": "a", "body": "b"}), &ctx)
            .await;
        assert!(!up.success, "ユニットを core として更新できてはいけない");
        let rt = RetractMemoryCoreAction
            .execute(&json!({"core_id": u1}), &ctx)
            .await;
        assert!(!rt.success, "ユニットを core として取り消せてはいけない");
    }

    /// 段階2 でタグ整理側が作りうる `node_type='meta'` / `source_type='category'` の行は、
    /// 凝縮の 3 経路（list / update / retract）から**見えない・触れない**。source_type ガードを
    /// 外すと落ちる（変異検出）。
    #[tokio::test]
    async fn core_tools_ignore_category_meta_rows() {
        let (_d, ctx) = test_context();
        let (u1, _u2) = seed_two_units(&ctx).await;

        // 本物の凝縮（source_type='condensed'）を 1 件作る。
        let r = RecordMemoryCoreAction
            .execute(
                &json!({"axis": "軸", "body": "本文", "sources": [u1]}),
                &ctx,
            )
            .await;
        assert!(r.success);
        let condensed_short = r.data.unwrap()["short_id"].as_str().unwrap().to_string();

        // タグ整理側が作る想定の category-meta 行を直接差し込む（node_type='meta' だが condensed でない）。
        {
            let conn = ctx.db.lock().unwrap();
            opencrab_db::queries::insert_index_node(
                &conn,
                &opencrab_db::queries::IndexNodeRow {
                    id: "meta-category-x".to_string(),
                    agent_id: "agent-1".to_string(),
                    parent_id: None,
                    node_type: "meta".to_string(),
                    source_type: "category".to_string(),
                    title: "カテゴリ meta".to_string(),
                    summary: "タグ整理側の meta".to_string(),
                    start_log_id: None,
                    end_log_id: None,
                    source_session_id: None,
                    date_from: None,
                    date_to: None,
                    depth: 1,
                    child_count: 0,
                    token_count: 0,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    short_id: Some("cat1".to_string()),
                    keywords_json: "[]".to_string(),
                    summary_refreshed_at: None,
                },
            )
            .unwrap();
        }

        // (1) list からは category-meta が見えない（condensed の 1 件だけ）。
        {
            let conn = ctx.db.lock().unwrap();
            let cores = opencrab_db::queries::list_memory_cores(&conn, "agent-1").unwrap();
            assert_eq!(cores.len(), 1, "list は condensed だけを返す");
            assert_eq!(cores[0].short_id.as_deref(), Some(condensed_short.as_str()));
        }

        // (2) update は category-meta を書き換えられない。
        let up = UpdateMemoryCoreAction
            .execute(&json!({"core_id": "cat1", "axis": "x", "body": "y"}), &ctx)
            .await;
        assert!(
            !up.success,
            "category-meta を凝縮として更新できてはいけない"
        );

        // (3) retract は category-meta を消せない。
        let rt = RetractMemoryCoreAction
            .execute(&json!({"core_id": "cat1"}), &ctx)
            .await;
        assert!(
            !rt.success,
            "category-meta を凝縮として取り消せてはいけない"
        );

        // category-meta は無傷で残っている。
        {
            let conn = ctx.db.lock().unwrap();
            assert!(
                opencrab_db::queries::get_index_node(&conn, "meta-category-x")
                    .unwrap()
                    .is_some(),
                "category-meta 行は凝縮道具に触られず残る"
            );
        }
    }

