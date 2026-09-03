pub(super) mod cases {
    use super::super::dispatcher::cases::FakeExecutor;
    use super::super::lifecycle::cases::{
        completed_log_body, completed_log_count, wait_until_settled,
    };
    use super::super::sink::cases::{dispatch_one, RecordingSink};
    use super::super::*;

    /// [P0 回帰] 同一バッチの複数ツールは 1 subtask 内で**dispatch 順に逐次実行**され、
    /// 完了 sink は **1 回だけ**発火する（N 通の返信にならない）。
    #[tokio::test]
    async fn batch_runs_sequentially_in_order_and_settles_once() {
        /// 実行順を記録し、最初のツールだけ遅い executor
        /// （個別 spawn だと速い方が先に完走して順序が崩れる）。
        struct OrderExecutor {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl ActionExecutor for OrderExecutor {
            async fn execute(&self, name: &str, _args: &serde_json::Value) -> ActionResult {
                if name == "slow_tool" {
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                }
                self.order.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!({"tool": name}),
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
        let order = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn ActionExecutor> = Arc::new(OrderExecutor {
            order: order.clone(),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );

        let calls = vec![
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
        ];
        let outcome = dispatcher.dispatch_batch(&calls);
        // バッチ全体で subtask は 1 本だけ。
        assert_eq!(registry.len(), 1, "1 バッチ = 1 subtask");
        assert!(outcome.label.contains("slow_tool"));
        assert!(outcome.label.contains("fast_tool"));

        wait_until_settled(&registry).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["slow_tool", "fast_tool"],
            "遅い方が先に dispatch されていれば先に実行される（並行化して順序を失わない）"
        );
        assert_eq!(
            sink.events.lock().unwrap().len(),
            1,
            "1 親ターンの resume は 1 回だけ"
        );
        assert_eq!(
            completed_log_count(&db, parent),
            1,
            "完了ログもバッチにつき 1 件"
        );
        // 本文には両ツールの結果が入る（resume 時に DB から読み直される）。
        let body = completed_log_body(&db, parent);
        assert!(body.contains("slow_tool") && body.contains("fast_tool"));
    }

    /// 指定名のツールだけ永久に pending する executor（残りは即成功）。
    /// 完走した call 数を数える（cancel 時の部分結果検証用）。
    struct HangingExecutor {
        hang_on: String,
        finished: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ActionExecutor for HangingExecutor {
        async fn execute(&self, name: &str, _args: &serde_json::Value) -> ActionResult {
            if name == self.hang_on {
                std::future::pending::<()>().await;
            }
            self.finished.lock().unwrap().push(name.to_string());
            ActionResult {
                success: true,
                data: serde_json::json!({"tool": name}),
                error: None,
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            Vec::new()
        }
    }

    fn call(tool: &str, id: &str) -> DispatchCall {
        DispatchCall {
            tool_name: tool.to_string(),
            args: serde_json::json!({"x": 1}),
            tool_call_id: id.to_string(),
        }
    }

    /// [P2 回帰] timeout でバッチが打ち切られたとき、**未実行 call も本文に現れる**。
    ///
    /// system prompt は「同じツールを再呼びするな（もう走っている）」と指示するので、
    /// 痕跡が無いとエージェントは未実行を知る手段が無く依頼が無言で消える。
    #[tokio::test]
    async fn timed_out_batch_records_skipped_calls() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(HangingExecutor {
            hang_on: "hangs".to_string(),
            finished: Arc::new(Mutex::new(Vec::new())),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_timeout(std::time::Duration::from_millis(60));

        dispatcher.dispatch_batch(&[
            call("ok1", "tc-1"),
            call("hangs", "tc-2"),
            call("ok2", "tc-3"),
            call("ok3", "tc-4"),
        ]);
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        // 4 call すべてが本文に現れる（未実行の 2 つは skipped として）。
        for id in ["tc-1", "tc-2", "tc-3", "tc-4"] {
            assert!(body.contains(id), "{id} が完了本文に無い: {body}");
        }
        assert_eq!(
            body.matches("skipped: batch timed out").count(),
            2,
            "未実行 call（ok2 / ok3）が skipped として記録されるべき: {body}"
        );
        assert_eq!(sink.events.lock().unwrap()[0].exit_reason, "timeout");
    }

    /// [P2 回帰] 複数ツールバッチの完了本文は **構造として** 結果を埋め、
    /// `tool_call_id` を含む（三重エスケープと順序依存の対応付けの解消）。
    #[tokio::test]
    async fn batch_body_embeds_results_as_json_with_tool_call_id() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(HangingExecutor {
            hang_on: "never".to_string(),
            finished: Arc::new(Mutex::new(Vec::new())),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        // 同じツールを 2 回呼ぶ（順序でしか対応が取れなかったケース）。
        dispatcher.dispatch_batch(&[call("ws_write", "tc-a"), call("ws_write", "tc-b")]);
        wait_until_settled(&registry).await;

        // 完了ログ本文（`{"type":"subtask_completed",...,"result":"<配列 JSON>"}`）を
        // 2 段でパースし、結果が**文字列ではなく object** であることを確かめる。
        let log: serde_json::Value =
            serde_json::from_str(&completed_log_body(&db, parent)).expect("完了ログは JSON");
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(log["result"].as_str().expect("result は配列 JSON 文字列"))
                .expect("result は JSON 配列としてパースできる");
        assert_eq!(arr.len(), 2);
        for (i, id) in ["tc-a", "tc-b"].iter().enumerate() {
            assert_eq!(arr[i]["tool"], "ws_write");
            assert_eq!(
                arr[i]["tool_call_id"], *id,
                "tool_call_id が無いと同名ツールの対応が取れない: {arr:?}"
            );
            assert!(
                arr[i]["result"].is_object(),
                "結果は構造として埋める（文字列だと多重エスケープになる）: {}",
                arr[i]["result"]
            );
            assert_eq!(arr[i]["result"]["success"], true);
        }
    }

    /// #551: 個々のツール結果は per-tool 上限内でも、複数ツールの**結合本文**が上限を
    /// 超えると `tool_result` と同じく workspace へ退避し、DB 本文には notice（パス・行数・
    /// 読み方）だけを残す。エージェントは notice の指すファイルから全文を読み返せる。
    #[tokio::test]
    async fn large_batch_result_is_offloaded_and_recoverable() {
        // 各ツールは per-tool 上限（TOOL_RESULT_TOKEN_LIMIT）内だが、2 本の結合で超える。
        struct BigExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for BigExecutor {
            async fn execute(&self, name: &str, _args: &serde_json::Value) -> ActionResult {
                ActionResult {
                    success: true,
                    // 変化のあるテキスト（連続同一文字だと tiktoken で潰れて上限に届かない）。
                    data: serde_json::json!({ "tool": name, "blob": "a1b2c3d4 ".repeat(250) }),
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
        let dir = tempfile::TempDir::new().unwrap();
        let executor: Arc<dyn ActionExecutor> = Arc::new(BigExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_workspace_root(Some(dir.path().to_path_buf()));

        dispatcher.dispatch_batch(&[call("ws_read", "tc-a"), call("ws_read", "tc-b")]);
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        let log: serde_json::Value = serde_json::from_str(&body).expect("完了ログは JSON");
        let result = log["result"].as_str().expect("result は文字列");

        // 本文には生データではなく退避 notice が載る（`tool_result` と同じ書式）。
        assert!(
            result.contains("withheld") && result.contains("tmp/"),
            "結合本文が退避されず生で載っている: {}",
            &result[..result.len().min(200)]
        );
        // 生の巨大 blob（片方ぶんでも）が本文へ漏れていない。
        assert!(
            !result.contains(&"a1b2c3d4 ".repeat(250)),
            "退避したのに生データが本文へ漏れている"
        );
        // speaker_id は None のまま（#501 の除外条件 system+heartbeat に掛からないこと）。
        {
            let conn = db.lock().unwrap();
            let row = opencrab_db::queries::list_recent_session_logs(&conn, parent, 50)
                .unwrap()
                .into_iter()
                .find(|l| l.content.contains("subtask_completed"))
                .unwrap();
            assert!(
                row.speaker_id.is_none(),
                "subtask 完了行の speaker_id は None のまま"
            );
        }
        // notice が指すファイル（workspace/tmp 配下）から全文を読み返せる＝回収可能。
        let tmp = dir.path().join("tmp");
        let files: Vec<_> = std::fs::read_dir(&tmp)
            .expect("tmp ディレクトリが作られる")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1, "退避ファイルが 1 つ作られる");
        let full = std::fs::read_to_string(files[0].path()).expect("退避ファイルを読み返せる");
        assert_eq!(
            full.matches(&"a1b2c3d4 ".repeat(250)).count(),
            2,
            "退避ファイルに両ツールの全結果が残っている（回収経路）"
        );
    }

    /// [P2 回帰] cancel でバッチを止めたとき、**完走済み call の部分結果**が親ログに残る
    /// （どこまで進んだかがラベルしか残らないのを防ぐ）。
    #[tokio::test]
    async fn cancel_records_partial_results_of_completed_calls() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        let finished = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn ActionExecutor> = Arc::new(HangingExecutor {
            hang_on: "hangs".to_string(),
            finished: finished.clone(),
        });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        let outcome = dispatcher.dispatch_batch(&[
            call("ws_write", "tc-1"),
            call("ws_write", "tc-2"),
            call("hangs", "tc-3"),
        ]);

        // 先頭 2 call が完走してハングに入るまで待つ。
        for _ in 0..200 {
            if finished.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(finished.lock().unwrap().len(), 2);

        let cancelled = cancel_subtask(
            &registry,
            &db,
            Some(sink.as_ref()),
            None,
            &outcome.subtask_id,
            CallerIdentity::Owner,
            None,
        );
        assert_eq!(cancelled, CancelOutcome::Cancelled);

        // 完了 sink は発火しない（停止したので返信しない）が、部分結果は残る。
        assert!(sink.events.lock().unwrap().is_empty());
        let conn = db.lock().unwrap();
        let log = opencrab_db::queries::list_recent_session_logs(&conn, parent, 10)
            .unwrap()
            .into_iter()
            .find(|l| l.log_type == "tool_cancelled")
            .expect("tool_cancelled が親ログに残る");
        assert!(
            log.content.contains("2 completed tool call(s)"),
            "完走済み call 数が残るべき: {}",
            log.content
        );
        assert!(log.content.contains("tc-1") && log.content.contains("tc-2"));
        assert!(
            !log.content.contains("tc-3"),
            "未完了 call は部分結果に含めない: {}",
            log.content
        );
        let meta: serde_json::Value =
            serde_json::from_str(log.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(meta["completed_calls"].as_array().unwrap().len(), 2);
    }

    /// [P0 回帰] dispatch にもタイムアウトがあり、`exit_reason="timeout"` で
    /// settle して registry から除去される（永久滞留＝無言の消失を防ぐ）。
    #[tokio::test]
    async fn dispatch_times_out_and_settles() {
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = Arc::new(DashMap::new());
        let sink = Arc::new(RecordingSink::default());
        let parent = "web-agent-a-conv1";
        // 完了しないツール。
        let executor: Arc<dyn ActionExecutor> = Arc::new(FakeExecutor { pending: true });

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_timeout(std::time::Duration::from_millis(60));

        dispatch_one(&dispatcher, "hangs_forever", serde_json::json!({}), "tc-1");
        wait_until_settled(&registry).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].exit_reason, "timeout",
            "既定タイムアウト超過は exit_reason=timeout で到達する"
        );
        assert_eq!(completed_log_count(&db, parent), 1);
    }

    /// 既定のタイムアウトは `spawn_subtask` と揃える。
    #[test]
    fn default_dispatch_timeout_matches_spawn_subtask() {
        assert_eq!(DEFAULT_DISPATCH_TIMEOUT_SECS, 1800);
    }

    /// [P1 回帰] ツールが panic しても `exit_reason="error"` で settle され、
    /// registry に死骸が残らない（REST が永久 active にならない）。
    #[tokio::test]
    async fn dispatch_panic_settles_as_error() {
        struct PanicExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for PanicExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                panic!("boom inside tool");
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
        let executor: Arc<dyn ActionExecutor> = Arc::new(PanicExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        dispatch_one(&dispatcher, "panics", serde_json::json!({}), "tc-1");

        wait_until_settled(&registry).await;
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1, "panic でも settle して通知する");
        assert_eq!(events[0].exit_reason, "error");
        assert!(completed_log_body(&db, parent).contains("panicked"));
    }

    /// [P1 回帰] dispatch 経路も inline と同じ無害化を通す:
    /// 大きい結果はワークスペースへ退避し、DB にはメタ情報だけを残す（#294）。
    #[tokio::test]
    async fn dispatch_offloads_large_result_like_inline() {
        struct BigExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for BigExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                ActionResult {
                    success: true,
                    data: serde_json::json!({"blob": "Z".repeat(50_000)}),
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
        let dir = tempfile::TempDir::new().unwrap();
        let parent = "web-agent-a-conv1";
        let executor: Arc<dyn ActionExecutor> = Arc::new(BigExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        )
        .with_workspace_root(Some(dir.path().to_path_buf()));

        dispatch_one(&dispatcher, "read_file", serde_json::json!({}), "tc-big");
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        assert!(
            body.contains("Tool result withheld") && body.contains("tmp/"),
            "上限超過はファイルへ退避してメタ情報だけ残す: {}",
            &body[..body.len().min(300)]
        );
        assert!(
            !body.contains("ZZZ"),
            "巨大本文（プレビューを含む）が session_logs に入ってはならない"
        );
        assert!(dir.path().join("tmp").read_dir().unwrap().count() > 0);
    }

    /// #620: dispatch 経路の永続化は **nsec キー名マスクをしない**（`SECRET_KEYS` を撤去した）。
    ///
    /// キー名一致は実際の混入（別の文字列値の中に鍵が含まれる形）を検出できず、`nsec` を
    /// JSON キーに持つ結果を返す producer も皆無だった（列挙で確認）。鍵は at-rest 暗号化と
    /// 実行時 env 注入で「読める範囲の外」に置く方式へ移した。ここは合成の nsec-keyed 結果が
    /// **マスクされず**そのまま永続化される（旧マスクが復活していないこと＝撤去の固定）ことと、
    /// 永続化そのものは従来どおり動くことを見る。
    #[tokio::test]
    async fn dispatch_persists_result_without_key_name_masking() {
        struct KeyLikeExecutor;
        #[async_trait::async_trait]
        impl ActionExecutor for KeyLikeExecutor {
            async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
                ActionResult {
                    success: true,
                    data: serde_json::json!({"npub": "npub1ok", "nsec": "nsec1synthetic"}),
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
        let executor: Arc<dyn ActionExecutor> = Arc::new(KeyLikeExecutor);

        let dispatcher = SubtaskToolDispatcher::new(
            executor,
            registry.clone(),
            db.clone(),
            sink.clone(),
            "agent-a",
            parent,
        );
        dispatch_one(
            &dispatcher,
            "nostr_generate_key",
            serde_json::json!({}),
            "tc-1",
        );
        wait_until_settled(&registry).await;

        let body = completed_log_body(&db, parent);
        // 撤去したはずのキー名マスクが効いていない（`[redacted]` を付けない）。
        assert!(
            !body.contains("redacted"),
            "撤去したはずのキー名マスクが復活している: {}",
            &body[..body.len().min(300)]
        );
        // 永続化そのものは従来どおり動く（結果が session_logs に載る）。
        assert!(body.contains("npub1ok"), "結果が永続化されていない");
    }
}
