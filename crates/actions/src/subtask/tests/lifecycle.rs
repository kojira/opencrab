
    // -----------------------------------------------------------------------
    // レビュー指摘（P0/P1）の回帰テスト群
    // -----------------------------------------------------------------------

    /// 親セッションログに着地した subtask_completed の件数。
    pub(crate) fn completed_log_count(db: &opencrab_db::Db, session_id: &str) -> usize {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_recent_session_logs(&conn, session_id, 50)
            .unwrap()
            .iter()
            .filter(|l| l.content.contains("subtask_completed"))
            .count()
    }

    /// 親セッションログの subtask_completed 本文（最初の 1 件）。
    pub(crate) fn completed_log_body(db: &opencrab_db::Db, session_id: &str) -> String {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_recent_session_logs(&conn, session_id, 50)
            .unwrap()
            .into_iter()
            .find(|l| l.content.contains("subtask_completed"))
            .map(|l| l.content)
            .unwrap_or_default()
    }

    /// settle が終わる（registry が空になる）まで待つ。
    pub(crate) async fn wait_until_settled(registry: &SubtaskRegistry) {
        for _ in 0..400 {
            if registry.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("subtask が決着しなかった（registry が空にならない）");
    }

    /// ラッチ: 停止と決着は排他（先に主張した一方だけが成功する）。
    #[test]
    fn lifecycle_claims_are_mutually_exclusive() {
        let l = SubtaskLifecycle::new();
        assert!(l.claim_cancel());
        assert!(!l.claim_settle(), "cancel 済みなら settle は主張できない");
        assert!(l.is_cancelled());

        let l2 = SubtaskLifecycle::new();
        assert!(l2.claim_settle());
        assert!(!l2.claim_cancel(), "決着済みなら cancel は主張できない");
        assert!(l2.is_settling());
    }

    /// [P0 回帰] cancel が先に主張していたら `settle_completed` は
    /// **DB 記録も sink 発火もしない**（止めたのに返信が届くのを防ぐ）。
    #[tokio::test]
    async fn settle_after_cancel_persists_nothing_and_fires_no_sink() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = RecordingSink::default();
        let parent = "web-agent-a-conv1";

        let lifecycle = SubtaskLifecycle::new();
        assert!(lifecycle.claim_cancel(), "cancel が先に主張する");

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: parent.to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "st-1".to_string(),
                sub_session_id: "subtask-st-1".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle,
            },
            "the result body",
        );

        assert_eq!(
            sink.events.lock().unwrap().len(),
            0,
            "cancel 後は完了 sink を発火しない"
        );
        assert_eq!(
            completed_log_count(&db, parent),
            0,
            "cancel 後は subtask_completed を DB へ書かない"
        );
    }

    /// [P0 回帰] 実経路: ツールが**完走した直後**（settle の DB 永続化より前）に
    /// `cancel_subtask` が入っても、完了ログも sink 発火も起きない。
    ///
    /// 競合窓を決定的に再現するため、executor が結果を返す直前に自分で
    /// `cancel_subtask` を呼ぶ（= tool 完走 → cancel → settle の順序）。
    #[tokio::test]
    async fn cancel_in_settle_window_suppresses_completion() {
        /// 結果を返す直前に自分の subtask を cancel する executor。
        struct CancellingExecutor {
            registry: SubtaskRegistry,
            db: opencrab_db::Db,
            outcome: Arc<Mutex<Option<CancelOutcome>>>,
        }
        #[async_trait::async_trait]
        impl ActionExecutor for CancellingExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                // 走行中の自分（registry の唯一のエントリ）を停止する。
                let id = self
                    .registry
                    .iter()
                    .next()
                    .map(|e| e.key().clone())
                    .expect("dispatch した subtask が registry にある");
                let outcome = cancel_subtask(
                    &self.registry,
                    &self.db,
                    None,
                    None,
                    &id,
                    CallerIdentity::Owner,
                    None,
                );
                *self.outcome.lock().unwrap() = Some(outcome);
                ActionResult {
                    success: true,
                    data: serde_json::json!({"ok": true}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                Vec::new()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let cancel_outcome = Arc::new(Mutex::new(None));

        let executor: Arc<dyn ActionExecutor> = Arc::new(CancellingExecutor {
            registry: registry.clone(),
            db: db.clone(),
            outcome: cancel_outcome.clone(),
        });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

        wait_until_settled(&registry).await;
        // settle 側が走り切るのを待つ（発火するならこの間に発火する）。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            *cancel_outcome.lock().unwrap(),
            Some(CancelOutcome::Cancelled),
            "cancel は成功を返している"
        );
        assert_eq!(
            sink.events.lock().unwrap().len(),
            0,
            "cancel 成功後に完了 sink が発火してはならない（resume して返信が届く）"
        );
        assert_eq!(
            completed_log_count(&db, parent),
            0,
            "cancel 成功後に subtask_completed が DB へ書かれてはならない"
        );
    }

    /// auto-dispatch は登録した subtask を通常どおり完了させる。
    #[tokio::test]
    async fn dispatch_still_completes() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());

        let dispatcher = SubtaskToolDispatcher::new(
            Arc::new(FakeExecutor { pending: false }) as Arc<dyn ActionExecutor>,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            "discord-agent-a-c1",
        );
        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");
        wait_until_settled(&registry).await;
        assert_eq!(
            sink.events.lock().unwrap().len(),
            1,
            "完了 sink が発火する"
        );
    }

    /// [P1 回帰] cancel は完了経路ではなく `on_subtask_cancelled` を通り、
    /// `exit_reason="cancelled"` / `kind=Cancelled` で通知される
    /// （REST が最後の subtask 停止でセッションを完了にできる）。
    #[tokio::test]
    async fn cancel_notifies_sink_without_completion() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = RecordingSink::default();
        let parent = "agent-msg-agent-a-u1";
        let handle = insert_fake_subtask(&registry, "st-1", parent);

        let outcome = cancel_subtask(
            &registry,
            &db,
            Some(&sink),
            None,
            "st-1",
            CallerIdentity::Agent,
            Some(parent),
        );
        assert_eq!(outcome, CancelOutcome::Cancelled);

        let cancelled = sink.cancelled.lock().unwrap();
        assert_eq!(cancelled.len(), 1, "停止は sink へ 1 回通知される");
        assert_eq!(cancelled[0].exit_reason, "cancelled");
        assert_eq!(cancelled[0].kind, SettleKind::Cancelled);
        assert_eq!(cancelled[0].session_id, parent);
        // 完了経路（resume する側）は発火しない。
        assert!(
            sink.events.lock().unwrap().is_empty(),
            "停止で on_subtask_settled（resume 経路）を呼んではならない"
        );
        assert!(registry.is_empty());
        handle.abort();
    }
