pub(super) mod cases {
    use super::super::*;

    /// `SubtaskCompletionSink` の最小フェイク実装。受け取った settle を記録する。
    #[derive(Default)]
    pub(crate) struct RecordingSink {
        pub(crate) events: Mutex<Vec<SubtaskSettled>>,
        /// 停止通知（`on_subtask_cancelled`）を別に記録する。
        pub(crate) cancelled: Mutex<Vec<SubtaskSettled>>,
    }

    // ---- #638: 継続の判断が transport 非依存であること（core 側の 1 本） ----

    /// 継続の判断に使う最小の sink。`prefix` / `progress` を差し替えて、
    /// **transport の性質だけで**判断が決まることを見る。
    #[derive(Clone)]
    struct ProbeSink {
        prefix: &'static str,
        progress: bool,
        delivered: Arc<Mutex<Vec<SettleKind>>>,
    }

    impl ProbeSink {
        fn new(prefix: &'static str, progress: bool) -> ProbeSink {
            ProbeSink {
                prefix,
                progress,
                delivered: Arc::new(Mutex::new(vec![])),
            }
        }
        fn kinds(&self) -> Vec<SettleKind> {
            self.delivered.lock().unwrap().clone()
        }
    }

    impl SubtaskCompletionSink for ProbeSink {
        fn session_prefix(&self) -> &'static str {
            self.prefix
        }
        fn forwards_progress(&self) -> bool {
            self.progress
        }
        fn deliver_continuation(&self, ev: SubtaskSettled) {
            self.delivered.lock().unwrap().push(ev.kind);
        }
    }

    fn probe_ev(session_id: &str, kind: SettleKind) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "a".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
            kind,
            reply_target: None,
            caller: crate::traits::CallerIdentity::Owner,
        }
    }

    /// **完了は transport に関わらず継続する**（#638 の中心）。接頭辞だけ違う 4 つの
    /// transport（discord / web / nostr / agent-msg）で、同じ判断が同じ結果になる。
    /// 以前は sink ごとに判断が書かれていて **REST にだけ継続が無かった**（#631 の実測）。
    #[test]
    fn completion_continues_on_every_transport() {
        for prefix in ["discord-", "web-", "nostr-", "agent-msg-"] {
            let sink = ProbeSink::new(prefix, false);
            let sid = format!("{prefix}agent-x");
            dispatch_settled(&sink, probe_ev(&sid, SettleKind::Completed));
            assert_eq!(
                sink.kinds(),
                vec![SettleKind::Completed],
                "{prefix} で完了の継続が起きていない（transport 非依存であること）"
            );
        }
    }

    /// **進捗は `forwards_progress()` が true の transport にだけ**配送される
    /// （Discord の進捗実況。web / Nostr / REST は完了だけ）。判断は 1 箇所で、
    /// transport は性質を名乗るだけ。
    #[test]
    fn progress_follows_the_transport_property_only() {
        let forwards = ProbeSink::new("discord-", true);
        dispatch_settled(&forwards, probe_ev("discord-a", SettleKind::Progress));
        assert_eq!(forwards.kinds(), vec![SettleKind::Progress]);

        let quiet = ProbeSink::new("web-", false);
        dispatch_settled(&quiet, probe_ev("web-a", SettleKind::Progress));
        assert!(
            quiet.kinds().is_empty(),
            "進捗を転送しない transport へ配送された"
        );
    }

    /// **停止（Cancelled）では継続しない**。停止は `on_subtask_cancelled` の役目で、
    /// ここへ流すと「止めたのに返信する」ことになる（既存の意図を集約後も保つ）。
    #[test]
    fn cancelled_never_continues() {
        let sink = ProbeSink::new("web-", true);
        dispatch_settled(&sink, probe_ev("web-a", SettleKind::Cancelled));
        assert!(sink.kinds().is_empty(), "停止で継続が起きた");
    }

    /// **他の transport の親セッションは配送しない**（ネストした subtask や heartbeat の
    /// 決着が同じ sink を通り得る）。
    #[test]
    fn foreign_parent_session_is_skipped() {
        let sink = ProbeSink::new("web-", false);
        dispatch_settled(&sink, probe_ev("heartbeat-a", SettleKind::Completed));
        dispatch_settled(&sink, probe_ev("subtask-a", SettleKind::Completed));
        assert!(sink.kinds().is_empty(), "他 transport の決着で継続が起きた");
    }

    impl SubtaskCompletionSink for RecordingSink {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, ev: SubtaskSettled) {
            self.events.lock().unwrap().push(ev);
        }
        fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
            self.cancelled.lock().unwrap().push(ev);
        }
    }

    /// 単一ツールを 1 バッチとして dispatch するテストヘルパ
    /// （engine は `dispatch_batch` しか呼ばない）。
    pub(crate) fn dispatch_one(
        dispatcher: &SubtaskToolDispatcher,
        tool_name: &str,
        args: serde_json::Value,
        tool_call_id: &str,
    ) -> DispatchOutcome {
        dispatcher.dispatch_batch(&[DispatchCall {
            tool_name: tool_name.to_string(),
            args,
            tool_call_id: tool_call_id.to_string(),
        }])
    }

    #[test]
    fn sink_receives_settled_event() {
        let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(RecordingSink::default());
        dispatch_settled(
            &*sink,
            SubtaskSettled {
                session_id: "discord-123".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "sub-1".to_string(),
                exit_reason: "completed".to_string(),
                kind: SettleKind::Completed,
                reply_target: None,
                caller: CallerIdentity::Agent,
            },
        );

        // downcast せずに検証するため、具象型で1つ生成しても振る舞いを確認できる。
        let recording = RecordingSink::default();
        dispatch_settled(
            &recording,
            SubtaskSettled {
                session_id: "nostr-abc".to_string(),
                agent_id: "agent-b".to_string(),
                subtask_id: "sub-2".to_string(),
                exit_reason: "progress".to_string(),
                kind: SettleKind::Progress,
                reply_target: None,
                caller: CallerIdentity::Agent,
            },
        );
        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subtask_id, "sub-2");
        assert_eq!(events[0].kind, SettleKind::Progress);
    }

    /// `settle_completed` は sink 発火の時点で subtask_completed ログが DB に
    /// 着地済みであること（順序契約 = RFC §6 受け入れ基準）を検証する。
    #[tokio::test]
    async fn settle_completed_persists_before_sink() {
        use std::sync::atomic::{AtomicI64, Ordering};

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());

        // 完了本体の代わりに、即完了しない pending task で abort_handle を用意。
        let handle = tokio::spawn(std::future::pending::<()>()).abort_handle();
        registry.insert(
            "sub-1".to_string(),
            SpawnedSubtask {
                abort_handle: handle,
                session_id: "subtask-sub-1".to_string(),
                parent_session_id: "discord-a-1-2".to_string(),
                agent_id: "agent-a".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: CallerIdentity::Agent,
                lifecycle: SubtaskLifecycle::new(),
                steerable: false,
            },
        );

        // sink は発火された瞬間の DB 上の subtask_completed ログ件数を記録する。
        struct OrderingSink {
            db: opencrab_db::Db,
            session_id: String,
            logs_at_fire: AtomicI64,
        }
        impl SubtaskCompletionSink for OrderingSink {
            fn session_prefix(&self) -> &'static str {
                ""
            }
            fn forwards_progress(&self) -> bool {
                true
            }
            fn deliver_continuation(&self, _ev: SubtaskSettled) {
                let conn = self.db.lock().unwrap();
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
                        [&self.session_id],
                        |row| row.get(0),
                    )
                    .unwrap();
                self.logs_at_fire.store(n, Ordering::SeqCst);
            }
        }
        let sink = OrderingSink {
            db: db.clone(),
            session_id: "discord-a-1-2".to_string(),
            logs_at_fire: AtomicI64::new(-1),
        };

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: "discord-a-1-2".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "sub-1".to_string(),
                sub_session_id: "subtask-sub-1".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "the result body",
        );

        // sink 発火時点で完了ログが既に DB にあった（DB 永続化 → 通知）。
        assert_eq!(sink.logs_at_fire.load(Ordering::SeqCst), 1);
        // registry からは除去済み。
        assert!(registry.is_empty());
    }

    /// #553: settle_completed は sub-session の `sessions.status` を **exit_reason** へ
    /// 遷移させ（'active' のままにしない）、親セッション（別モード）の status には触れない。
    #[tokio::test]
    async fn settle_completed_transitions_sub_session_status() {
        let conn = opencrab_db::init_memory().unwrap();
        let mk = |id: &str, mode: &str| opencrab_db::queries::SessionRow {
            id: id.to_string(),
            mode: mode.to_string(),
            theme: "Subtask: t".to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: "[]".to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        };
        // sub-session 行（active）と親 discord セッション（active）を用意。
        opencrab_db::queries::insert_session(&conn, &mk("subtask-sub-9", "subtask")).unwrap();
        opencrab_db::queries::insert_session(&conn, &mk("discord-a-1-2", "discord")).unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());

        let handle = tokio::spawn(std::future::pending::<()>()).abort_handle();
        registry.insert(
            "sub-9".to_string(),
            SpawnedSubtask {
                abort_handle: handle,
                session_id: "subtask-sub-9".to_string(),
                parent_session_id: "discord-a-1-2".to_string(),
                agent_id: "agent-a".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: CallerIdentity::Agent,
                lifecycle: SubtaskLifecycle::new(),
                steerable: false,
            },
        );

        struct NoopSink;
        impl SubtaskCompletionSink for NoopSink {
            fn session_prefix(&self) -> &'static str {
                ""
            }
            fn forwards_progress(&self) -> bool {
                true
            }
            fn deliver_continuation(&self, _ev: SubtaskSettled) {}
        }

        settle_completed(
            &registry,
            &db,
            &NoopSink,
            SettleContext {
                parent_session_id: "discord-a-1-2".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "sub-9".to_string(),
                sub_session_id: "subtask-sub-9".to_string(),
                // "completed" 固定ではなく実 exit_reason を書くことを固定する。
                exit_reason: "timeout".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "body",
        );

        let conn = db.lock().unwrap();
        // sub-session は exit_reason へ終端化されている。
        assert_eq!(
            opencrab_db::queries::get_session(&conn, "subtask-sub-9")
                .unwrap()
                .unwrap()
                .status,
            "timeout"
        );
        // 親（別モード）の status は不変。
        assert_eq!(
            opencrab_db::queries::get_session(&conn, "discord-a-1-2")
                .unwrap()
                .unwrap()
                .status,
            "active"
        );
    }

    /// 与えた `reply_target` で fake subtask を登録し、`settle_completed` を通した
    /// ときに sink が受け取った `SubtaskSettled` を返すヘルパ。
    ///
    /// 「DB 永続化 → registry 除去 → sink 発火」の順序契約もここで併せて検証する
    /// （sink 発火時点で完了ログが着地済み・registry から除去済み）。
    async fn settle_and_capture(reply_target: Option<&str>) -> SubtaskSettled {
        settle_and_capture_as(reply_target, CallerIdentity::Agent).await
    }

    /// `settle_and_capture` の呼び出し元指定版（#298）。
    async fn settle_and_capture_as(
        reply_target: Option<&str>,
        caller: CallerIdentity,
    ) -> SubtaskSettled {
        use std::sync::atomic::{AtomicI64, Ordering};

        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let parent = "nostr-agent-a-npub1sender";

        let handle = tokio::spawn(std::future::pending::<()>()).abort_handle();
        registry.insert(
            "sub-rt".to_string(),
            SpawnedSubtask {
                abort_handle: handle,
                session_id: "subtask-sub-rt".to_string(),
                parent_session_id: parent.to_string(),
                agent_id: "agent-a".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: reply_target.map(|s| s.to_string()),
                caller,
                lifecycle: SubtaskLifecycle::new(),
                steerable: false,
            },
        );

        // sink 発火の瞬間に「完了ログ件数 / registry に残っているか」も記録する。
        struct CapturingSink {
            db: opencrab_db::Db,
            registry: SubtaskRegistry,
            session_id: String,
            event: Mutex<Option<SubtaskSettled>>,
            logs_at_fire: AtomicI64,
            still_registered: std::sync::atomic::AtomicBool,
        }
        impl SubtaskCompletionSink for CapturingSink {
            fn session_prefix(&self) -> &'static str {
                ""
            }
            fn forwards_progress(&self) -> bool {
                true
            }
            fn deliver_continuation(&self, ev: SubtaskSettled) {
                let n: i64 = {
                    let conn = self.db.lock().unwrap();
                    conn.query_row(
                        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
                        [&self.session_id],
                        |row| row.get(0),
                    )
                    .unwrap()
                };
                self.logs_at_fire.store(n, Ordering::SeqCst);
                self.still_registered
                    .store(self.registry.contains_key(&ev.subtask_id), Ordering::SeqCst);
                *self.event.lock().unwrap() = Some(ev);
            }
        }

        let sink = CapturingSink {
            db: db.clone(),
            registry: registry.clone(),
            session_id: parent.to_string(),
            event: Mutex::new(None),
            logs_at_fire: AtomicI64::new(-1),
            still_registered: std::sync::atomic::AtomicBool::new(true),
        };

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: parent.to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "sub-rt".to_string(),
                sub_session_id: "subtask-sub-rt".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "the result body",
        );

        // 順序契約: sink 発火時点で DB 永続化済み・registry 除去済み。
        assert_eq!(
            sink.logs_at_fire.load(Ordering::SeqCst),
            1,
            "sink 発火より前に subtask_completed が DB へ着地している"
        );
        assert!(
            !sink.still_registered.load(Ordering::SeqCst),
            "sink 発火より前に registry から除去されている"
        );
        assert!(registry.is_empty());

        let captured = sink.event.lock().unwrap().take();
        captured.expect("sink が発火する")
    }

    /// #167: settle 時に registry の `reply_target` を読み出して sink へ渡す。
    /// 除去より前に回収するため、remove 後でも値が失われない。
    #[tokio::test]
    async fn settle_completed_passes_reply_target_to_sink() {
        let ev = settle_and_capture(Some("nostr:note1abcdef")).await;
        assert_eq!(ev.reply_target.as_deref(), Some("nostr:note1abcdef"));
        assert_eq!(ev.kind, SettleKind::Completed);
        assert_eq!(ev.subtask_id, "sub-rt");
    }

    /// #298: settle 時に registry の `caller`（= subtask を spawn した親 run の
    /// 呼び出し元）を読み出して sink へ渡す。resume する sink はこれで元の権限のまま
    /// 親ターンを再開できる。落とすと owner/trusted のツールが `policy_allows` で
    /// list_tools からも dispatch からも消える。
    #[tokio::test]
    async fn settle_completed_passes_caller_to_sink() {
        let ev = settle_and_capture_as(None, CallerIdentity::Owner).await;
        assert_eq!(
            ev.caller,
            CallerIdentity::Owner,
            "決着通知が呼び出し元を落としている（resume が最小権限へ降格する）"
        );

        // 昇格経路は作らない: 元が Agent なら Agent のまま。
        let ev = settle_and_capture_as(None, CallerIdentity::Agent).await;
        assert_eq!(ev.caller, CallerIdentity::Agent);
    }

    /// #167 非退行: `reply_target` が None（Discord 経路）なら None のまま渡り、
    /// 従来どおり session_id から返信先を復元する sink の挙動を変えない。
    #[tokio::test]
    async fn settle_completed_without_reply_target_yields_none() {
        let ev = settle_and_capture(None).await;
        assert!(ev.reply_target.is_none());
        assert_eq!(ev.exit_reason, "completed");
    }

    /// registry に該当エントリが無い（既に cancel された等）場合も従来どおり
    /// sink は発火し、`reply_target` は None になる。
    #[tokio::test]
    async fn settle_completed_missing_registry_entry_yields_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = RecordingSink::default();

        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: "web-agent-a-c1".to_string(),
                agent_id: "agent-a".to_string(),
                subtask_id: "gone".to_string(),
                sub_session_id: "subtask-gone".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "body",
        );

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].reply_target.is_none());
    }
}
