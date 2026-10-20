
    /// 単一ツールを即完了（または永久 pending）で返す最小 executor。
    /// `SubtaskToolDispatcher` の配線検証用（合成 executor は別テストで検証済み）。
    pub(crate) struct FakeExecutor {
        pub(crate) pending: bool,
    }

    #[async_trait::async_trait]
    impl ActionExecutor for FakeExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            if self.pending {
                std::future::pending::<()>().await;
            }
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
                ..Default::default()
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            Vec::new()
        }
    }

    /// #167: `SubtaskToolDispatcher::with_reply_target` の値が dispatch した
    /// subtask の `SpawnedSubtask.reply_target` に載る。
    #[tokio::test]
    async fn dispatcher_sets_reply_target_on_spawned_subtask() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            "nostr-agent-a-npub1sender",
        )
        .with_reply_target(Some("nostr:note1target".to_string()));

        let outcome = dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");
        let entry = registry.get(&outcome.subtask_id).unwrap();
        assert_eq!(entry.reply_target.as_deref(), Some("nostr:note1target"));
        entry.abort_handle.abort();
        drop(entry);
    }

    /// #167 非退行: `with_reply_target` を呼ばない（Discord 経路）と従来どおり None。
    #[tokio::test]
    async fn dispatcher_defaults_reply_target_to_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            "discord-agent-a-1-2",
        );

        let outcome = dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");
        let entry = registry.get(&outcome.subtask_id).unwrap();
        assert!(entry.reply_target.is_none());
        entry.abort_handle.abort();
        drop(entry);
    }

    /// #167: `RunRequest::with_reply_target` の値が（`process.rs` と同じ配線で）
    /// dispatcher → `SpawnedSubtask` → settle → sink まで一貫して運ばれる。
    #[tokio::test]
    async fn run_request_reply_target_reaches_sink_via_dispatcher() {
        use crate::traits::CallerIdentity;
        use crate::RunRequest;

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());

        // ゲートウェイ側（Nostr 等）が inbound の返信先を RunRequest に載せる。
        let req = RunRequest::new(
            "agent-a",
            "A",
            "nostr-agent-a-npub1sender",
            "sys",
            "conv",
            "nostr",
            CallerIdentity::Agent,
        )
        .with_reply_target("nostr:note1abcdef")
        .with_dispatch(Some(registry.clone()), sink.clone());
        assert_eq!(req.reply_target.as_deref(), Some("nostr:note1abcdef"));

        // process.rs の dispatcher 構築と同じ配線（RunRequest → dispatcher）。
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            req.agent_id.clone(),
            req.session_id.clone(),
        )
        .with_reply_target(req.reply_target.clone());

        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

        // settle（DB 永続化 → 除去 → sink 発火）まで待つ。
        for _ in 0..200 {
            if !sink.events.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].reply_target.as_deref(),
            Some("nostr:note1abcdef"),
            "RunRequest の reply_target が settle 時に sink まで届く"
        );
        assert_eq!(events[0].session_id, "nostr-agent-a-npub1sender");
    }

    /// #298: `RunRequest.caller` が（`process.rs` と同じ配線で）dispatcher →
    /// `SpawnedSubtask` → settle → sink まで一貫して運ばれる。
    ///
    /// 非ブロック dispatch は**普通のツール呼び出し**を background 化するので、
    /// オーナー発のターンでツールを 1 つ呼んだだけで resume が起きる。ここで
    /// 呼び出し元が落ちると、その resume 以降 owner/trusted のツールが丸ごと消える。
    #[tokio::test]
    async fn run_request_caller_reaches_sink_via_dispatcher() {
        use crate::RunRequest;

        for caller in [CallerIdentity::Owner, CallerIdentity::Agent] {
            let conn = opencrab_db::init_memory().unwrap();
            let db = opencrab_db::Db::from_connection(conn);
            let registry: SubtaskRegistry = Arc::new(DashMap::new());
            let sink = Arc::new(RecordingSink::default());

            let req = RunRequest::new(
                "agent-a",
                "A",
                "discord-agent-a-1-2",
                "sys",
                "conv",
                "discord",
                caller.clone(),
            )
            .with_dispatch(Some(registry.clone()), sink.clone());

            let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });
            let dispatcher = SubtaskToolDispatcher::new(
                executor,
                registry.clone(),
                db.clone(),
                sink.clone(),
                req.agent_id.clone(),
                req.session_id.clone(),
            )
            .with_caller(req.caller.clone());

            dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

            for _ in 0..200 {
                if !sink.events.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].caller, caller,
                "RunRequest の caller が settle 時に sink まで届いていない"
            );
        }
    }

    /// #167: `NoopCompletionSink` は sink 実装を書かずに `with_dispatch`
    /// （sink 必須 API）を満たせる。呼んでもログのみで何もしない。
    #[tokio::test]
    async fn noop_completion_sink_enables_dispatch_without_reinjection() {
        use crate::traits::CallerIdentity;
        use crate::RunRequest;

        let req = RunRequest::new(
            "agent-a",
            "A",
            "heartbeat-agent-a",
            "sys",
            "conv",
            "heartbeat",
            CallerIdentity::Agent,
        )
        .with_dispatch(None, Arc::new(NoopCompletionSink));
        // dispatch が有効化される（process.rs は completion_sink が Some のときだけ
        // dispatcher を注入する）。
        assert!(req.completion_sink.is_some());

        // dispatcher の sink として使え、settle まで通る（再注入はしない）。
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            req.completion_sink.clone().unwrap(),
            "agent-a",
            "heartbeat-agent-a",
        );
        dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");

        for _ in 0..200 {
            if registry.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(registry.is_empty(), "settle 後に registry から除去される");

        // 再注入はしないが、完了本文は DB へ永続化される（次 tick で拾える）。
        let conn = db.lock().unwrap();
        let logs =
            opencrab_db::queries::list_recent_session_logs(&conn, "heartbeat-agent-a", 10).unwrap();
        assert!(
            logs.iter().any(|l| l.content.contains("subtask_completed")),
            "NoopCompletionSink でも完了ログは DB へ着地する"
        );
    }

    /// #923 §2.7: describe_tools は既定の非 dispatch 集合に含まれる（inline 固定の種）。
    /// `inline_tool_names` はこの集合を種にするので、ここに入れることで
    /// `should_dispatch("describe_tools") == false` が保証される。現 tip では未登録で **赤**。
    #[test]
    fn describe_tools_is_in_default_non_dispatch_tools() {
        assert!(
            default_non_dispatch_tools().contains("describe_tools"),
            "describe_tools が default_non_dispatch_tools に無い（dispatch されると活性集合が親 executor に届かない）"
        );
    }

    /// RFC #152 S3a + S2 dormant 解消の実経路実証:
    /// dispatch した単一ツール（`nostr_generate_key`）が**合成 executor**
    /// （`BridgedExecutor` + gateway_actions = server ツール源）で実行され、完了が
    /// `settle_completed`（DB 永続化 → registry 除去 → sink 発火）で親セッションへ
    /// 再注入されること。
    #[tokio::test]
    async fn dispatched_single_tool_runs_on_composite_executor_and_reinjects() {
        use crate::bridge::BridgedExecutor;
        use crate::dispatcher::ActionDispatcher;
        use crate::traits::{ActionContext, CallerIdentity};
        use opencrab_gateway::{
            GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
        };

        // `nostr_generate_key`（長時間 = Dispatchable）と、配送系の inline ツール数種を
        // 提供する mock 合成 gateway（server ツール源の代役）。分類の権威は各定義の
        // `class.dispatch` 属性なので、非同期化除外の検証には実属性を持つ定義が要る。
        // nsec は返さず npub/pubkey のみ返す（実装と同じく秘密は LLM へ出さない）。
        struct MockServerGateway;
        #[async_trait::async_trait]
        impl GatewayActions for MockServerGateway {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                let inline = opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                };
                let mut defs = vec![GatewayActionDef {
                    name: "nostr_generate_key".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Dispatchable,
                        sub_engine: opencrab_gateway::SubEngineAccess::Allowed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "generate a nostr key".to_string(),
                    parameters: serde_json::json!({"type":"object"}),
                }];
                // 配送系 inline（本番の gateway 定義と同じ属性）。索引に載って非同期化から
                // 除外されることを end-to-end で確認するために定義に含める。
                for name in ["discord_send_file", "send_ui", "nostr_reply", "nostr_post"] {
                    defs.push(GatewayActionDef {
                        name: name.to_string(),
                        class: inline,
                        description: format!("{name} delivery tool"),
                        parameters: serde_json::json!({"type":"object"}),
                    });
                }
                defs
            }
            async fn execute(
                &self,
                name: &str,
                _args: &serde_json::Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                assert_eq!(name, "nostr_generate_key");
                GatewayActionResult {
                    success: true,
                    data: Some(serde_json::json!({"npub":"npub1abc","pubkey":"deadbeef"})),
                    error: None,
                }
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let parent = "discord-agent-x-1-2";
        let ctx = ActionContext {
            caller: CallerIdentity::Agent,
            agent_id: "agent-x".to_string(),
            agent_name: "X".to_string(),
            session_id: Some(parent.to_string()),
            db: db.clone(),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(Mutex::new(None)),
            model_override: Arc::new(Mutex::new(None)),
            current_purpose: Arc::new(Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "discord".to_string(),
            })),
        };

        // 合成 executor（gateway_actions に server ツール源）を 1 つの Arc にまとめる。
        let executor: Arc<dyn ActionExecutor> = Arc::new(
            BridgedExecutor::new(ActionDispatcher::new(), ctx)
                .with_gateway_actions(Arc::new(MockServerGateway)),
        );

        let sink = Arc::new(RecordingSink::default());
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-x",
            parent,
        );

        // dispatch 対象判定: server ツールは dispatch、制御系/配送系はしない。
        assert!(dispatcher.should_dispatch("nostr_generate_key"));
        assert!(!dispatcher.should_dispatch("spawn_subtask"));
        // 実在する Discord 配送系（`discord_send` は現行 gateway に無い死名だった）。
        assert!(!dispatcher.should_dispatch("discord_send_file"));
        assert!(!dispatcher.should_dispatch("send_ui"));
        // Nostr 配送系（#168）: background 化すると暗黙返信と二重投稿になる。
        // （nostr_dm は #514 で撤去したのでここでは検証しない。）
        assert!(!dispatcher.should_dispatch("nostr_reply"));
        assert!(!dispatcher.should_dispatch("nostr_post"));
        // #923 §2.7: describe_tools は inline（non-dispatch）必須。dispatch すると子 executor
        // へ detach され、活性化したツール集合が親の list_tools に反映されない（実測・
        // DIRECTION-LOG 508）。should_dispatch=false を観測境界で固定する。現 tip では
        // describe_tools が非 dispatch 集合に無く should_dispatch=true なので **赤**。
        assert!(
            !dispatcher.should_dispatch("describe_tools"),
            "describe_tools が dispatch 対象になっている（inline 固定でなければ活性化が親に届かない）"
        );

        let outcome = dispatch_one(
            &dispatcher,
            "nostr_generate_key",
            serde_json::json!({}),
            "tc-1",
        );
        assert!(outcome.label.starts_with("nostr_generate_key("));

        // 完了待ち: settle_completed が registry から remove するまで。
        for _ in 0..200 {
            if registry.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(registry.is_empty(), "settle 後に registry から除去される");

        // sink が completed で 1 回だけ発火（再注入トリガ）。
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].exit_reason, "completed");
        assert_eq!(events[0].kind, SettleKind::Completed);
        assert_eq!(events[0].session_id, parent);
        drop(events);

        // DB へ subtask_completed が着地し、result にツール結果（npub）を含む
        // （resume は build_conversation_string でこれを読み直す = RFC §1.3）。
        let conn = db.lock().unwrap();
        let logs = opencrab_db::queries::list_recent_session_logs(&conn, parent, 10).unwrap();
        assert!(
            logs.iter().any(|l| {
                l.content.contains("subtask_completed") && l.content.contains("npub1abc")
            }),
            "親セッションログに subtask_completed（result=npub 含む）が永続化される"
        );
    }

    /// RFC #152 S3a / P0: auto-dispatch した subtask は**共有 registry** に載り、
    /// その `abort_handle` で停止できること（`cancel_subtask` の認可ゲートが叩く経路）。
    #[tokio::test]
    async fn dispatched_subtask_is_registered_and_abortable() {
        use crate::bridge::BridgedExecutor;
        use crate::dispatcher::ActionDispatcher;
        use crate::traits::{ActionContext, CallerIdentity};
        use opencrab_gateway::{
            GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
        };

        // 実行が完了しない（pending）ツールを提供する gateway。abort されるまで走り続ける。
        struct BlockingGateway;
        #[async_trait::async_trait]
        impl GatewayActions for BlockingGateway {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![GatewayActionDef {
                    name: "long_running".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "never completes".to_string(),
                    parameters: serde_json::json!({"type":"object"}),
                }]
            }
            async fn execute(
                &self,
                _name: &str,
                _args: &serde_json::Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let parent = "discord-agent-x-1-2";
        let ctx = ActionContext {
            caller: CallerIdentity::Agent,
            agent_id: "agent-x".to_string(),
            agent_name: "X".to_string(),
            session_id: Some(parent.to_string()),
            db: db.clone(),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(Mutex::new(None)),
            model_override: Arc::new(Mutex::new(None)),
            current_purpose: Arc::new(Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "discord".to_string(),
            })),
        };
        let executor: Arc<dyn ActionExecutor> = Arc::new(
            BridgedExecutor::new(ActionDispatcher::new(), ctx)
                .with_gateway_actions(Arc::new(BlockingGateway)),
        );

        let sink = Arc::new(RecordingSink::default());
        // 共有 registry（ループと gateway_actions が共有するものの代役）。
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-x",
            parent,
        );

        let outcome = dispatch_one(&dispatcher, "long_running", serde_json::json!({}), "tc-1");

        // 共有 registry に載っている（＝cancel_subtask から到達可能）。
        assert!(registry.contains_key(&outcome.subtask_id));
        let entry = registry.get(&outcome.subtask_id).unwrap();
        assert_eq!(entry.parent_session_id, parent);

        // cancel_subtask 相当: abort_handle で停止 → registry から除去。
        entry.abort_handle.abort();
        drop(entry);
        registry.remove(&outcome.subtask_id);

        // 完了で settle しないので sink は発火しない（aborted = 完了イベント無し）。
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(registry.is_empty());
        assert!(
            sink.events.lock().unwrap().is_empty(),
            "abort された subtask は settle_completed を通らず sink を発火しない"
        );
    }

    /// D-1087: 背景 run の観測文脈。executor が受け取った `(session_id, depth)` を記録する。
    struct RunRecordingExecutor {
        seen: Arc<Mutex<Vec<(String, u32, String)>>>,
    }

    #[async_trait::async_trait]
    impl ActionExecutor for RunRecordingExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
                ..Default::default()
            }
        }
        async fn execute_with_id(
            &self,
            name: &str,
            args: &serde_json::Value,
            tool_call_id: &str,
        ) -> ActionResult {
            let _ = (name, args, tool_call_id);
            panic!("背景 run は execute_in_run を通ること（文脈なしで実行しない）");
        }
        async fn execute_in_run(
            &self,
            run: opencrab_core::ExecutorRun<'_>,
            name: &str,
            _args: &serde_json::Value,
            tool_call_id: &str,
        ) -> ActionResult {
            self.seen.lock().unwrap().push((
                run.session_id.to_string(),
                run.depth,
                tool_call_id.to_string(),
            ));
            let _ = name;
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
                ..Default::default()
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            Vec::new()
        }
    }

    /// D-1087: 自動 dispatch された**背景バッチ**も、サブタスクと同じライフサイクル通知を
    /// 出す（開始 → 終端）。run の相関キーは、そのバッチのツール実行が観測に載る
    /// `ExecutorRun.session_id` と同じ値でなければならない（gateway が両者を突き合わせる）。
    #[tokio::test]
    async fn dispatched_batch_emits_lifecycle_with_the_run_key_of_its_tools() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn ActionExecutor> = Arc::new(RunRecordingExecutor {
            seen: seen.clone(),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            "web-agent-a-conv1",
        )
        .with_depth(2);

        let outcome = dispatcher.dispatch_batch(&[
            DispatchCall {
                tool_name: "slow_tool".to_string(),
                args: serde_json::json!({"path": "x"}),
                tool_call_id: "tc-1".to_string(),
            },
            DispatchCall {
                tool_name: "fast_tool".to_string(),
                args: serde_json::json!({"cmd": "build"}),
                tool_call_id: "tc-2".to_string(),
            },
        ]);
        wait_until_settled(&registry).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // ツール実行は「このバッチの run」の文脈で走る（親は depth 2 なので 3）。
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].0, format!("subtask-{}", outcome.subtask_id));
        assert_eq!(seen[0].1, 3);
        assert_eq!(seen[0].2, "tc-1");
        assert_eq!(seen[1].2, "tc-2");

        // ライフサイクル: 開始 → 終端（この順で 1 回ずつ）。
        let statuses: Vec<(SubtaskLifecycleStatus, String)> = sink
            .lifecycle
            .lock()
            .unwrap()
            .iter()
            .map(|(status, id, _)| (*status, id.clone()))
            .collect();
        assert_eq!(
            statuses,
            vec![
                (SubtaskLifecycleStatus::Started, outcome.subtask_id.clone()),
                (SubtaskLifecycleStatus::Completed, outcome.subtask_id.clone()),
            ],
            "{statuses:?}"
        );
        // 開始も終端も、ツール実行と同じ run のセッション ID を運ぶ。
        let sessions: Vec<String> = sink
            .lifecycle_session
            .lock()
            .unwrap()
            .iter()
            .map(|(session, _)| session.clone())
            .collect();
        assert_eq!(
            sessions,
            vec![format!("subtask-{}", outcome.subtask_id); 2],
            "{sessions:?}"
        );
        // 終端は所要時間を持つ。
        assert!(
            sink.lifecycle.lock().unwrap()[1].2.is_some(),
            "completed carries the elapsed time"
        );
        // 終端は結果本文を持つ（従来の決着本文と同じ）。
        assert!(sink.lifecycle_session.lock().unwrap()[1].1.is_some());
    }

    /// D-1087: 観測を購読していない（`NoopCompletionSink`）ときは、背景バッチの決着も
    /// 実行結果も従来どおり（ライフサイクル通知は捨てられるだけで、走行は変わらない）。
    #[tokio::test]
    async fn dispatched_batch_without_an_observer_settles_unchanged() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(NoopCompletionSink);
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: false });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink,
            "agent-a",
            "web-agent-a-conv1",
        );
        let outcome = dispatch_one(&dispatcher, "some_tool", serde_json::json!({}), "tc-1");
        wait_until_settled(&registry).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let body = completed_log_body(&db, "web-agent-a-conv1");
        assert!(body.contains("ok"), "{body}");
        assert!(outcome.subtask_id.len() > 8);
    }
