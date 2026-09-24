
    struct CountingExecutor(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl ActionExecutor for CountingExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    fn s6_relationship_fixture() -> (opencrab_db::Db, opencrab_core::authorization::RelationshipAuthority) {
        let db = opencrab_db::Db::memory().unwrap();
        {
            let conn = db.lock().unwrap();
            opencrab_db::queries::insert_trusted_co_agent(
                &conn,
                &opencrab_db::queries::TrustedCoAgentRow {
                    id: "relationship-1".into(),
                    agent_id: "agent-a".into(),
                    co_agent_id: "peer-agent".into(),
                    allowed_actions: None,
                    created_by: "owner".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    relationship_revision: 1,
                    active: true,
                },
            )
            .unwrap();
        }
        (
            db,
            opencrab_core::authorization::RelationshipAuthority {
                co_agent_id: "peer-agent".into(),
                relationship_revision: 1,
            },
        )
    }

    fn s6_current_check(
        db: opencrab_db::Db,
        authority: opencrab_core::authorization::RelationshipAuthority,
    ) -> opencrab_core::authorization::AuthorizationCheck {
        Arc::new(move |_| {
            let conn = db.lock().unwrap();
            opencrab_db::queries::co_agent_relationship_is_current(
                &conn,
                "agent-a",
                &authority.co_agent_id,
                authority.relationship_revision,
            )
            .unwrap()
        })
    }

    fn s6_mutate_relationship(db: &opencrab_db::Db, mutation: &str) {
        let conn = db.lock().unwrap();
        match mutation {
            "revoke" => assert!(opencrab_db::queries::delete_trusted_co_agent(
                &conn,
                "agent-a",
                "peer-agent",
            )
            .unwrap()),
            "revision_bump" => assert!(opencrab_db::queries::bump_trusted_co_agent_revision(
                &conn,
                "agent-a",
                "peer-agent",
            )
            .unwrap()),
            _ => unreachable!(),
        }
    }

    async fn s6_wait_for_subtask_removal(registry: &SubtaskRegistry, subtask_id: &str) {
        for _ in 0..200 {
            if !registry.contains_key(subtask_id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("subtask did not settle: {subtask_id}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn s6_real_queue_dequeue_and_concurrent_release_revalidate_current_relationship() {
        for mutation in ["revoke", "revision_bump"] {
            // Queued retry: stale carried evidence is present before the production dequeue.
            let (db, authority) = s6_relationship_fixture();
            s6_mutate_relationship(&db, mutation);
            let registry: SubtaskRegistry = Arc::new(DashMap::new());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let dispatcher = SubtaskToolDispatcher::new(
                Arc::new(CountingExecutor(calls.clone())),
                registry.clone(),
                db.clone(),
                Arc::new(RecordingSink::default()),
                "agent-a",
                "parent-session",
            )
            .with_authorization_check(Some(s6_current_check(db, authority)));
            let outcome = dispatch_one(
                &dispatcher,
                "some_tool",
                serde_json::json!({}),
                "queued-tool-call",
            );
            s6_wait_for_subtask_removal(&registry, &outcome.subtask_id).await;
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "{mutation}:queued");

            // Concurrent release: enqueue while current, then mutate before the production
            // dequeue check is allowed to continue.
            let (db, authority) = s6_relationship_fixture();
            let registry: SubtaskRegistry = Arc::new(DashMap::new());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let executor: Arc<dyn ActionExecutor> = Arc::new(CountingExecutor(calls.clone()));
            let sink = Arc::new(RecordingSink::default());
            let arrived = Arc::new(std::sync::Barrier::new(2));
            let released = Arc::new(std::sync::Barrier::new(2));
            let base_check = s6_current_check(db.clone(), authority);
            let check: opencrab_core::authorization::AuthorizationCheck = {
                let arrived = arrived.clone();
                let released = released.clone();
                Arc::new(move |boundary| {
                    assert_eq!(
                        boundary,
                        opencrab_core::authorization::AuthorizationBoundary::QueueDequeueRetry
                    );
                    arrived.wait();
                    released.wait();
                    base_check(boundary)
                })
            };
            let dispatcher = SubtaskToolDispatcher::new(
                executor,
                registry.clone(),
                db.clone(),
                sink,
                "agent-a",
                "parent-session",
            )
            .with_authorization_check(Some(check));
            let outcome = dispatch_one(
                &dispatcher,
                "some_tool",
                serde_json::json!({}),
                "tool-call-1",
            );
            let mutator_db = db.clone();
            let arrived_thread = arrived.clone();
            let released_thread = released.clone();
            let mutation_name = mutation.to_string();
            let mutator = std::thread::spawn(move || {
                arrived_thread.wait();
                s6_mutate_relationship(&mutator_db, &mutation_name);
                released_thread.wait();
            });
            mutator.join().unwrap();
            s6_wait_for_subtask_removal(&registry, &outcome.subtask_id).await;
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "{mutation}:concurrent");
        }
    }
