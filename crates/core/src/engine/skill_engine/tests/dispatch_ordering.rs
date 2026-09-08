    /// [P0 回帰] 同一ターンに複数ツールが来たとき、tool_call ごとに個別 dispatch せず
    /// **1 本の subtask** にまとめること（順序保持 ＋ 完了通知＝親 resume の 1 回化）。
    #[tokio::test]
    async fn test_multi_tool_batch_dispatched_as_single_subtask() {
        use std::sync::atomic::Ordering as AtomicOrdering;
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "write_file", serde_json::json!({"path": "x"})),
                tc("tc-2", "execute_shell", serde_json::json!({"cmd": "build"})),
            ]),
            text_response("開始しました"),
        ]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        let dispatcher = Arc::new(RecordingDispatcher::new(&["spawn_subtask"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, _name, json, _err| {
            seen_clone.lock().unwrap().push(json);
        });

        let result = engine.run("system", "go", "test-model").await.unwrap();

        // subtask は 1 本だけ（= settle も sink 発火も 1 回）。
        assert_eq!(
            dispatcher.batches.load(AtomicOrdering::SeqCst),
            1,
            "同一バッチの複数ツールは 1 本の subtask にまとめる"
        );
        // dispatch 順序は LLM が並べた順のまま渡る。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["write_file,execute_shell"]
        );
        // tool_call ごとに spawned マーカーは返る（同じ subtask_id）。
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen
            .iter()
            .all(|s| s.contains("\"subtask_id\":\"sub-for-write_file+execute_shell\"")));
        assert_eq!(result.tool_calls_made, 2);
    }

    /// [P0 回帰] dispatch 不可のツールが 1 つでも混ざるバッチは**全体を inline 実行**し、
    /// LLM が並べた順序を保つ（分割すると inline と background の相対順序が崩れる）。
    #[tokio::test]
    async fn test_mixed_batch_falls_back_to_inline_in_order() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "write_file", serde_json::json!({"path": "x"})),
                tc("tc-2", "discord_send", serde_json::json!({"text": "hi"})),
            ]),
            text_response("done"),
        ]);
        struct OrderExecutor {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for OrderExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.order.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(OrderExecutor {
                order: order.clone(),
            }),
            10,
        );
        // discord_send は dispatch 不可（配送系）。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["discord_send"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        engine.run("system", "go", "test-model").await.unwrap();

        assert_eq!(
            dispatcher.dispatched.lock().unwrap().len(),
            0,
            "混在バッチは dispatch せず inline に落とす"
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["write_file", "discord_send"],
            "inline 実行は LLM が並べた順序を守る"
        );
    }

    /// [#671 回帰] inline 接頭辞 ＋ dispatch 可接尾辞の混在バッチは、接頭辞を同期実行して
    /// から接尾辞を **1 本の subtask** として dispatch する。実行順は「接頭辞 inline →
    /// 接尾辞 dispatch」で固定し、接尾辞は spawned マーカーを同ターンで返す。
    /// （実事故: `[record_task_progress(inline), execute_shell(31 分)]` が全体 inline に
    /// 落ちてロックを占有した縮退の再発防止。）
    #[tokio::test]
    async fn test_inline_prefix_then_dispatch_suffix_split() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::{Arc, Mutex};

        // record_task_progress(inline 分類) → execute_shell(dispatch 可) の順。
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc(
                    "tc-1",
                    "record_task_progress",
                    serde_json::json!({"note": "start"}),
                ),
                tc(
                    "tc-2",
                    "execute_shell",
                    serde_json::json!({"cmd": "claude ..."}),
                ),
            ]),
            text_response("開始しました"),
        ]);

        // executor（inline 実行）と dispatcher（subtask 化）を同一タイムラインへ記録し、
        // 「接頭辞 inline が接尾辞 dispatch より先に完了する」を固定する。
        let timeline: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        struct TimelineExecutor {
            tl: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for TimelineExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.tl.lock().unwrap().push(format!("inline:{name}"));
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }

        struct TimelineDispatcher {
            control: std::collections::HashSet<String>,
            tl: Arc<Mutex<Vec<String>>>,
            dispatched: Mutex<Vec<String>>,
            batches: AtomicUsize,
        }
        impl crate::ToolDispatcher for TimelineDispatcher {
            fn should_dispatch(&self, name: &str) -> bool {
                !self.control.contains(name)
            }
            fn dispatch_batch(&self, calls: &[crate::DispatchCall]) -> crate::DispatchOutcome {
                self.batches.fetch_add(1, AtomicOrdering::SeqCst);
                let names: Vec<String> = calls.iter().map(|c| c.tool_name.clone()).collect();
                self.tl
                    .lock()
                    .unwrap()
                    .push(format!("dispatch:{}", names.join("+")));
                self.dispatched.lock().unwrap().push(names.join(","));
                crate::DispatchOutcome {
                    subtask_id: format!("sub-for-{}", names.join("+")),
                    label: names.join(", "),
                }
            }
        }

        let executor = TimelineExecutor {
            tl: timeline.clone(),
        };
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let dispatcher = Arc::new(TimelineDispatcher {
            control: ["record_task_progress"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tl: timeline.clone(),
            dispatched: Mutex::new(Vec::new()),
            batches: AtomicUsize::new(0),
        });
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, name, json, _err| {
            seen_clone.lock().unwrap().push(format!("{name}:{json}"));
        });

        let result = engine.run("system", "go", "test-model").await.unwrap();

        // 接頭辞 inline → 接尾辞 dispatch の順で実行される（順序保証）。
        assert_eq!(
            timeline.lock().unwrap().as_slice(),
            &[
                "inline:record_task_progress".to_string(),
                "dispatch:execute_shell".to_string()
            ],
            "inline 接頭辞は接尾辞 dispatch より先に完了する"
        );
        // dispatch は接尾辞（execute_shell）だけを 1 本にまとめる。
        assert_eq!(dispatcher.batches.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["execute_shell"]
        );

        // tool_result: 接頭辞は inline 実結果、接尾辞は spawned マーカー（同ターン返却）。
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("record_task_progress:"));
        assert!(
            !seen[0].contains("\"status\":\"spawned\""),
            "接頭辞は inline 実行結果であって spawned マーカーではない"
        );
        assert!(seen[1].starts_with("execute_shell:"));
        assert!(
            seen[1].contains("\"status\":\"spawned\""),
            "接尾辞は spawned マーカー（同ターン返却）"
        );
        assert!(seen[1].contains("\"subtask_id\":\"sub-for-execute_shell\""));

        assert_eq!(result.tool_calls_made, 2);
        assert_eq!(result.response, "開始しました");
    }

    /// [#671 回帰] dispatch 可ツールの**後ろに** inline ツールが来る混在バッチは分割できず
    /// （inline と background の相対順序が保証できない）、従来どおり全体 inline に縮退する。
    /// このとき縮退原因のツール名を含む debug ログ（stage="batch_split"）を出す。
    ///
    /// 縮退ログの捕捉はスレッドローカル subscriber に依存するため、cargo の並列テストと
    /// 干渉しないよう、専用の current-thread ランタイムを `with_default` の内側で回す
    /// （`#[tokio::test]` だと subscriber の有効スレッドと polling スレッドがずれ得る）。
    #[test]
    fn test_dispatchable_then_inline_stays_whole_inline_and_logs() {
        use std::sync::{Arc, Mutex};

        // execute_shell(dispatch 可) → record_task_progress(inline) の順。
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "execute_shell", serde_json::json!({"cmd": "x"})),
                tc(
                    "tc-2",
                    "record_task_progress",
                    serde_json::json!({"note": "done"}),
                ),
            ]),
            text_response("done"),
        ]);
        struct OrderExecutor {
            order: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for OrderExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.order.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }

        // 縮退 debug ログを捕捉する。cargo の並列テストが触る tracing のグローバル
        // MAX_LEVEL と干渉しないよう、fmt を使わず**常時 enabled** の最小 Subscriber で
        // イベントのフィールドを直接拾う（`enabled` が常に true、`max_level_hint` は
        // 既定=TRACE なのでレベル早期棄却の影響を受けない）。
        struct FieldGrabber {
            out: String,
        }
        impl tracing::field::Visit for FieldGrabber {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.out.push_str(&format!("{}={:?};", field.name(), value));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.out.push_str(&format!("{}={};", field.name(), value));
            }
        }
        struct CaptureSubscriber {
            events: Arc<Mutex<Vec<String>>>,
        }
        impl tracing::Subscriber for CaptureSubscriber {
            fn enabled(&self, _md: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut grabber = FieldGrabber { out: String::new() };
                event.record(&mut grabber);
                self.events.lock().unwrap().push(grabber.out);
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            events: events.clone(),
        };

        let order = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(OrderExecutor {
                order: order.clone(),
            }),
            10,
        );
        // record_task_progress を inline（should_dispatch=false）扱いに。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["record_task_progress"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        // subscriber を有効化したまま、同一スレッドで run を完走させる。
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                engine.run("system", "go", "test-model").await.unwrap();
            });
        });

        // dispatch は起きず、全体 inline を LLM 順で実行する。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().len(),
            0,
            "dispatch 可の後ろに inline が来る混在バッチは dispatch せず全体 inline"
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["execute_shell", "record_task_progress"],
            "全体 inline は LLM が並べた順序を守る"
        );

        // 縮退ログが出ており、stage=batch_split と原因ツール名（record_task_progress）を含む。
        let logs = events.lock().unwrap().join("\n");
        assert!(
            logs.contains("batch_split"),
            "縮退 debug ログ（stage=batch_split）が出る: {logs}"
        );
        assert!(
            logs.contains("record_task_progress"),
            "縮退ログに原因の inline ツール名が載る: {logs}"
        );
    }

    /// [#671] 制御系 inline ツール（declare_done: ターン終了宣言）が接頭辞に来ても、
    /// エンジンのループ終了条件と矛盾しないことを固定する。declare_done を inline 実行し、
    /// 後続の execute_shell を背景 subtask 化して同ターンで spawned を返す。ターンの終了は
    /// 従来どおり「LLM が次イテレーションでツールを呼ばない」ことで駆動され、declare_done の
    /// `{done:true}` 結果はループを早期に切らない（engine は tool_result の done を見ない）。
    #[tokio::test]
    async fn test_control_inline_prefix_dispatches_suffix_and_loop_ends_on_llm() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc(
                    "tc-1",
                    "declare_done",
                    serde_json::json!({"reason": "十分議論した"}),
                ),
                tc(
                    "tc-2",
                    "execute_shell",
                    serde_json::json!({"cmd": "claude ..."}),
                ),
            ]),
            // 次イテレーションでツールを呼ばない → ここでループ終了（declare_done ではなく）。
            text_response("終わります"),
        ]);

        // executor は inline 実行だけを記録（declare_done のみ来るべき）。
        struct SpyExecutor {
            called: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for SpyExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.called.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!({"done": true}),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let called = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(SpyExecutor {
                called: called.clone(),
            }),
            10,
        );
        // declare_done を inline（should_dispatch=false）扱いに。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["declare_done"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, name, json, _err| {
            seen_clone.lock().unwrap().push(format!("{name}:{json}"));
        });

        let result = engine.run("system", "go", "test-model").await.unwrap();

        // declare_done だけ inline 実行、execute_shell は inline 実行されない。
        assert_eq!(called.lock().unwrap().as_slice(), &["declare_done"]);
        // execute_shell（接尾辞）が 1 本の subtask として dispatch される。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["execute_shell"]
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].starts_with("declare_done:"));
        assert!(
            seen[0].contains("\"done\":true"),
            "接頭辞は declare_done の inline 実行結果"
        );
        assert!(seen[1].starts_with("execute_shell:"));
        assert!(
            seen[1].contains("\"status\":\"spawned\""),
            "接尾辞は spawned マーカー（同ターン返却）"
        );

        // ループは declare_done では切れず、LLM が次イテレーションでツールを呼ばず終了する。
        assert_eq!(
            result.iterations, 2,
            "ターンは 2 イテレーションで正常終了する"
        );
        assert_eq!(result.response, "終わります");
    }

    /// [#671 挙動変化] 未許可ツールが接頭辞・dispatch 可ツールが接尾辞に来るバッチ
    /// （例: typo や権限落ちの 1 ツール）。**旧実装**は「1 つでも `is_action_allowed &&
    /// should_dispatch` を満たさない → 全体 inline」で execute_shell も inline に落ちていた。
    /// **新実装**は未許可ツールに permission denied を返した後、接尾辞を背景 subtask 化する
    /// （1 ツールの権限落ちが非ブロック性を壊さない）。denied の扱い自体は不変。
    #[tokio::test]
    async fn test_unauthorized_prefix_still_dispatches_suffix() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "not_a_real_tool", serde_json::json!({})),
                tc("tc-2", "execute_shell", serde_json::json!({"cmd": "x"})),
            ]),
            text_response("done"),
        ]);
        // executor は inline 実行のみ記録（未許可は executor に届かず denied になるべき）。
        struct SpyExecutor {
            called: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for SpyExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.called.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let called = Arc::new(Mutex::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(llm),
            Box::new(SpyExecutor {
                called: called.clone(),
            }),
            10,
        );
        // execute_shell のみ許可（not_a_real_tool は未許可 → is_action_allowed=false）。
        engine.set_allowed_actions(["execute_shell".to_string()]);
        // control 集合は空。execute_shell は dispatch 可。
        let dispatcher = Arc::new(RecordingDispatcher::new(&[]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let seen: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        engine.add_on_tool_result(move |_id, name, json, err| {
            seen_clone.lock().unwrap().push((name, json, err));
        });

        engine.run("system", "go", "test-model").await.unwrap();

        // 未許可ツールは executor に届かない（denied の扱い不変）。
        assert!(
            called.lock().unwrap().is_empty(),
            "未許可ツールは inline executor に渡らない"
        );
        // 接尾辞 execute_shell は inline に落ちず、背景 subtask として dispatch される（挙動変化）。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["execute_shell"],
            "未許可ツールが接頭辞にあっても dispatch 可接尾辞は subtask 化される"
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        // 接頭辞: permission denied（err=true・not authorized 文言）。従来どおり。
        assert_eq!(seen[0].0, "not_a_real_tool");
        assert!(seen[0].2, "未許可ツールは err=true で通知される");
        assert!(seen[0].1.contains("is not authorized"));
        // 接尾辞: spawned マーカー（同ターン・err=false）。
        assert_eq!(seen[1].0, "execute_shell");
        assert!(!seen[1].2);
        assert!(seen[1].1.contains("\"status\":\"spawned\""));
    }
