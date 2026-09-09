    // -----------------------------------------------------------------------
    // #890 §11: CONTINUE 末尾マーカーによるターン継続（TDD 赤テスト）。
    //
    // LLM 呼び出し回数（MockLlm::counting / MarkerCapturingLlm）とイテレーション数で
    // 構造計測する。文面分類は一切しない。マーカーは生成 content の末尾に置く。
    // -----------------------------------------------------------------------

    /// LLM 呼び出し回数と各リクエストを記録する計測用クライアント（#890）。
    struct MarkerCapturingLlm {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        requests: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait]
    impl LlmClient for MarkerCapturingLlm {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no more mock responses");
            }
            Ok(responses.remove(0))
        }
    }

    #[allow(clippy::type_complexity)]
    impl MarkerCapturingLlm {
        fn new(
            responses: Vec<ChatResponse>,
        ) -> (
            Self,
            Arc<std::sync::atomic::AtomicUsize>,
            Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        ) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    responses: std::sync::Mutex::new(responses),
                    calls: calls.clone(),
                    requests: requests.clone(),
                },
                calls,
                requests,
            )
        }
    }

    /// 全リクエストのメッセージ本文に CONTINUE が一切現れないこと（§11.6）。
    fn no_continue_in_requests(requests: &[ChatRequest]) -> bool {
        requests.iter().all(|req| {
            req.messages.iter().all(|m| {
                m.text_content()
                    .map(|t| !t.contains("CONTINUE"))
                    .unwrap_or(true)
            })
        })
    }

    /// (a) reply×N のみ → LLM 1 呼び出し・iterations 1（R7 維持・マーカー機構下でも不変）。
    #[tokio::test]
    async fn continue_marker_a_reply_only_completes_in_one_call() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls) = MockLlm::counting(vec![tool_call_response(vec![
            tc("reply-1", "reply", serde_json::json!({"text": "one"})),
            tc("reply-2", "reply", serde_json::json!({"text": "two"})),
            tc("reply-3", "reply", serde_json::json!({"text": "three"})),
        ])]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "3回に分けて返信して", "test-model")
            .await
            .expect("純発話生成は 1 呼び出しで完了する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.iterations, 1);
        assert_eq!(
            executor_calls.lock().unwrap().as_slice(),
            &["reply", "reply", "reply"]
        );
    }

    /// (b) 発話＋末尾 CONTINUE → 2 呼び出し目が走る・発話は 1 回だけ配送。
    #[tokio::test]
    async fn continue_marker_b_speech_then_marker_runs_second_call() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls) = MockLlm::counting(vec![
            resp(Some("ざっと見て感想を返すね⚡\nCONTINUE"), vec![]),
            text_response("読んだ。結論はXだが条件Yで再現性が弱い。"),
        ]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_cb = delivered.clone();
        engine.set_on_response_text(move |t: String| {
            delivered_cb.lock().unwrap().push(t);
        });

        let result = engine
            .run("system", "この論文どう思う？", "test-model")
            .await
            .expect("末尾 CONTINUE は次イテレーションで最終応答へ到達する");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            2,
            "末尾 CONTINUE が 2 回目の LLM 呼び出しを起こす"
        );
        assert_eq!(result.iterations, 2);
        assert_eq!(result.response, "読んだ。結論はXだが条件Yで再現性が弱い。");
        let delivered = delivered.lock().unwrap();
        assert_eq!(
            delivered.as_slice(),
            &[
                "ざっと見て感想を返すね⚡".to_string(),
                "読んだ。結論はXだが条件Yで再現性が弱い。".to_string(),
            ],
            "発話はマーカー除去後の本文を 1 回だけ配送する"
        );
    }

    /// (b2・#900) 発話クラスツール（reply）のみ＋末尾 CONTINUE → 発話配送後に次イテレーション。
    /// 純発話でも末尾 CONTINUE があれば R7 の 1 生成完結ではなく継続する（併記した CONTINUE を尊重）。
    /// reply×1＋CONTINUE → reply×1＋CONTINUE → reply×1 で配送 3・LLM 3・CONTINUE は本文へ残さない。
    #[tokio::test]
    async fn continue_marker_b2_utterance_only_with_marker_runs_next_iteration() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls) = MockLlm::counting(vec![
            resp(
                Some("CONTINUE"),
                vec![tc("reply-1", "reply", serde_json::json!({"text": "one"}))],
            ),
            resp(
                Some("CONTINUE"),
                vec![tc("reply-2", "reply", serde_json::json!({"text": "two"}))],
            ),
            tool_call_response(vec![tc(
                "reply-3",
                "reply",
                serde_json::json!({"text": "three"}),
            )]),
        ]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        // 発話は on_response_text ではなく executor 経由で配送される（純発話・本文 None）。
        // CONTINUE 単独 content は剥がされ空になるので say は 1 度も飛ばない。
        let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_cb = delivered.clone();
        engine.set_on_response_text(move |t: String| {
            delivered_cb.lock().unwrap().push(t);
        });

        let result = engine
            .run("system", "3回に分けて返信して", "test-model")
            .await
            .expect("純発話＋末尾 CONTINUE は次イテレーションで完了する");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            3,
            "reply＋CONTINUE は毎回 2 回目以降の LLM 呼び出しを起こす"
        );
        assert_eq!(result.iterations, 3);
        assert_eq!(
            executor_calls.lock().unwrap().as_slice(),
            &["reply", "reply", "reply"],
            "3 回の reply がすべて配送される"
        );
        // CONTINUE 単独 content は剥がされて say にならない（本文へ残らない）。
        assert!(
            delivered.lock().unwrap().is_empty(),
            "CONTINUE 単独 content が say として配送された: {:?}",
            delivered.lock().unwrap()
        );
    }

    /// (#8・§13) reply×N＋本文＋末尾 CONTINUE → reply を配送しつつ本文を配送して継続、次生成で終了。
    /// engine 契約の層で固定する（extgate の途中発話配送＝§12.2 は #898 の担当。ここは #900 が所有する
    /// 「継続機構＋本文の on_response_text 配送」を isol[ate] する）。1 生成目 reply(R1)＋本文A＋CONTINUE
    /// → on_response_text で本文A・executor で reply を配送し継続、2 生成目 本文B で自然終了。LLM 2。
    #[tokio::test]
    async fn continue_marker_reply_body_marker_delivers_body_and_reply_then_continues() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls) = MockLlm::counting(vec![
            resp(
                Some("本文A\nCONTINUE"),
                vec![tc("reply-1", "reply", serde_json::json!({"text": "R1"}))],
            ),
            text_response("本文B"),
        ]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_cb = delivered.clone();
        engine.set_on_response_text(move |t: String| {
            delivered_cb.lock().unwrap().push(t);
        });

        let result = engine
            .run("system", "返信しつつ続けて", "test-model")
            .await
            .expect("reply＋本文＋CONTINUE は次イテレーションで本文Bへ到達する");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            2,
            "reply＋本文＋末尾 CONTINUE が次イテレーションを起こさない（§13 #8=進む）"
        );
        assert_eq!(result.iterations, 2);
        // 本文（マーカー剥がし後）は各イテレーションで on_response_text 配送される（本文A→本文B）。
        assert_eq!(
            delivered.lock().unwrap().as_slice(),
            &["本文A".to_string(), "本文B".to_string()],
            "本文A/本文B が順に配送されない（CONTINUE 残留 or 継続失敗）"
        );
        // reply は executor 経由で 1 度だけ配送される。
        assert_eq!(executor_calls.lock().unwrap().as_slice(), &["reply"]);
    }

    /// (c) CONTINUE＋query ツール併記 → ツール経路で 2 呼び出し・二重継続なし・マーカーは剥がす。
    #[tokio::test]
    async fn continue_marker_c_with_query_tool_uses_tool_path_no_double() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls, requests) = MarkerCapturingLlm::new(vec![
            resp(
                Some("全文を確認する\nCONTINUE"),
                vec![tc("resolve-1", "resolve", serde_json::json!({"ref": "e1"}))],
            ),
            text_response("確認した"),
        ]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "全文を見て", "test-model")
            .await
            .expect("query ツール併記は従来の混在経路で継続する");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            2,
            "ツール経路が 2 回目を起こす（マーカーで 3 回目にはならない＝二重継続なし）"
        );
        assert_eq!(result.iterations, 2);
        assert!(
            no_continue_in_requests(&requests.lock().unwrap()),
            "併記時もマーカーは会話へ残さない"
        );
    }

    /// (d) CONTINUE 連打 → max_iterations で停止・fail-loud（max=3 で chat 3・iterations 4）。
    #[tokio::test]
    async fn continue_marker_d_spam_stops_at_max_iterations() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("CONTINUE"),
            text_response("CONTINUE"),
            text_response("CONTINUE"),
        ]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 3);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "続けて", "test-model")
            .await
            .expect("上限到達は Ok の打ち切り応答で返る");

        assert!(
            result.stopped_by_limit,
            "CONTINUE 連打は既存 max_iterations で fail-loud 停止する"
        );
        assert_eq!(chat_calls.load(Ordering::SeqCst), 3, "max=3 で LLM は 3 回");
        assert_eq!(result.iterations, 4, "4 周目の上限判定で停止する");
    }

    /// (e) 発話のみ（マーカー無し）→ 次呼び出し不発（R7 回帰・機構が空目覚めを起こさない）。
    #[tokio::test]
    async fn continue_marker_e_speech_only_no_second_call() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls) = MockLlm::counting(vec![tool_call_response(vec![tc(
            "reply-1",
            "reply",
            serde_json::json!({"text": "ただの返事"}),
        )])]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "返事して", "test-model")
            .await
            .expect("マーカー無し発話は 1 呼び出しで終わる");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            1,
            "マーカーが無ければ発話のみは次を呼ばない（R7）"
        );
        assert_eq!(result.iterations, 1);
        assert_eq!(executor_calls.lock().unwrap().as_slice(), &["reply"]);
    }

    /// (f) 途中出現（末尾以外）→ 剥がされず・継続せず（chat 1・本文そのまま）。
    #[tokio::test]
    async fn continue_marker_f_midtext_not_stripped_no_continue() {
        use std::sync::atomic::Ordering;

        const BODY: &str = "まず CONTINUE を確認してから作業します";
        let (llm, chat_calls) = MockLlm::counting(vec![resp(Some(BODY), vec![])]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "説明して", "test-model")
            .await
            .expect("途中出現は継続せず最終応答になる");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            1,
            "末尾以外の CONTINUE は継続の足がかりにしない"
        );
        assert_eq!(result.iterations, 1);
        assert_eq!(result.response, BODY, "途中出現は本文をそのまま残す");
    }

    /// (f2) §11.7: 同一行に他の文字がある CONTINUE は継続マーカーではない（末尾行が単独で
    /// ないため剥がさず・継続せず・本文そのまま）。chat 1。
    #[tokio::test]
    async fn continue_marker_f2_same_line_marker_not_continued() {
        use std::sync::atomic::Ordering;

        const BODY: &str = "確認して返信します CONTINUE";
        let (llm, chat_calls) =
            MockLlm::counting(vec![resp(Some(BODY), vec![]), text_response("二回目")]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "説明して", "test-model")
            .await
            .expect("同一行併記は継続せず最終応答になる");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            1,
            "最終行が CONTINUE 単独でなければ継続しない"
        );
        assert_eq!(result.iterations, 1);
        assert_eq!(
            result.response, BODY,
            "同一行併記は本文そのまま（マーカー扱いしない）"
        );
    }

    /// (g) NO_REPLY＋CONTINUE 同時末尾 → NO_REPLY 優先で終端（継続しない・chat 1）。
    #[tokio::test]
    async fn continue_marker_g_no_reply_wins_over_continue() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) =
            MockLlm::counting(vec![resp(Some("本文だけ話す NO_REPLY\nCONTINUE"), vec![])]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "どうする？", "test-model")
            .await
            .expect("NO_REPLY 優先で終端する");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            1,
            "NO_REPLY が末尾にあれば CONTINUE が同居しても継続しない"
        );
        assert_eq!(result.iterations, 1);
    }

    /// (h) 保存 speech と次イテレーションの会話文字列に CONTINUE が含まれない（§11.6）。
    #[tokio::test]
    async fn continue_marker_h_marker_absent_from_conversation() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls, requests) = MarkerCapturingLlm::new(vec![
            resp(Some("感想を返すね⚡\nCONTINUE"), vec![]),
            text_response("最終回答"),
        ]);
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let delivered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let delivered_cb = delivered.clone();
        engine.set_on_response_text(move |t: String| {
            delivered_cb.lock().unwrap().push(t);
        });

        let _ = engine
            .run("system", "論文見て", "test-model")
            .await
            .expect("継続後に最終応答へ到達する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        let delivered = delivered.lock().unwrap();
        assert!(
            delivered.iter().all(|t| !t.contains("CONTINUE")),
            "配送された speech にマーカーが残らない"
        );
        assert_eq!(
            delivered.first().map(String::as_str),
            Some("感想を返すね⚡"),
            "1 回目の配送はマーカー除去後の本文"
        );
        assert!(
            no_continue_in_requests(&requests.lock().unwrap()),
            "次イテレーションのプロンプト会話部分にマーカーが現れない"
        );
    }

    /// (h-typed) §11.6 + #884 PR2: typed 経路（typed_conversation 有り・typed_history=true）でも
    /// 保存前にマーカーが剥がされ、末尾 CONTINUE で継続し、次イテレーションの会話文字列に
    /// CONTINUE が現れない。
    #[tokio::test]
    async fn continue_marker_h_typed_history_marker_absent() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls, requests) = MarkerCapturingLlm::new(vec![
            resp(Some("感想を返すね⚡\nCONTINUE"), vec![]),
            text_response("最終回答"),
        ]);
        let typed_conversation = crate::conversation_typed::TypedConversation {
            context_block: None,
            snapshot_base: None,
            history: vec![Message {
                role: Role::User,
                content: Some(MessageContent::Text("typed current turn".to_string())),
                name: Some("owner".to_string()),
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            response_directive: Some("directive".to_string()),
            wire_tokens: 0,
            diagnostics: crate::conversation_typed::DeriveDiagnostics {
                item_count: 1,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            },
        };
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_conversation_waters(1, 0);
        engine.set_typed_conversation(Some(typed_conversation));

        let result = engine
            .run("system", "FLAT_HISTORY_SENTINEL", "test-model")
            .await
            .expect("typed 経路でも継続後に最終応答へ到達する");

        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            2,
            "typed 経路でも末尾 CONTINUE で 2 回目が走る"
        );
        assert_eq!(result.response, "最終回答");
        assert!(
            no_continue_in_requests(&requests.lock().unwrap()),
            "typed 経路でも会話文字列にマーカーが現れない（§11.6）"
        );
    }

    /// §13.1 f【sub-engine(depth>0)でも CONTINUE 有効・sub-engine の max_iterations で上限】
    /// sub-engine は退避先未設定・小さめ max の SkillEngine プロファイル。CONTINUE 機構は
    /// 共有ループにあり depth で gate されないので継続は効き、上限は sub-engine の max_iterations。
    /// 現 tip で緑（非回帰ピン）。
    #[tokio::test]
    async fn continue_marker_i_sub_engine_profile_continues_and_bounded() {
        use std::sync::atomic::Ordering;

        // (1) sub-engine プロファイル（退避先未設定・max=5）でも末尾 CONTINUE で継続する。
        let (llm, chat_calls) = MockLlm::counting(vec![
            resp(Some("下調べするね⚡\nCONTINUE"), vec![]),
            text_response("完了"),
        ]);
        let mut sub = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 5);
        sub.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let r = sub
            .run("system", "go", "test-model")
            .await
            .expect("sub-engine でも末尾 CONTINUE は次イテレーションへ進む");
        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            2,
            "sub-engine でも末尾 CONTINUE で 2 回目が走る"
        );
        assert_eq!(r.response, "完了");

        // (2) sub-engine の max_iterations（timeout と並ぶ上限の代表）で fail-loud 停止する。
        let (llm2, chat2) =
            MockLlm::counting(vec![text_response("CONTINUE"), text_response("CONTINUE")]);
        let mut sub2 = SkillEngine::new(Box::new(llm2), Box::new(MockExecutor::new()), 2);
        sub2.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let r2 = sub2
            .run("system", "go", "test-model")
            .await
            .expect("上限到達は Ok の打ち切り応答で返る");
        assert!(
            r2.stopped_by_limit,
            "sub-engine の max_iterations でも CONTINUE 連鎖は上限停止する"
        );
        assert_eq!(
            chat2.load(Ordering::SeqCst),
            2,
            "sub-engine max=2 で LLM 2 回"
        );
    }

    /// §13.1 a【空 CONTINUE 連続 3 回で warn 1 行（停止しない・解析用）】
    /// 現 tip: engine は空 CONTINUE 連鎖に対する解析 warn を出さない → CONTINUE_LOG_TARGET
    /// イベント 0 で**赤**。実装は 3 連続空生成＋CONTINUE で
    /// target=CONTINUE_LOG_TARGET("opencrab::continue_marker") に warn を 1 行出す
    /// （停止しない・上限は既存 max_iterations）。捕捉はスレッドローカル subscriber なので
    /// 専用 current-thread ランタイムを with_default の内側で回す（並列テストと非干渉）。
    #[test]
    fn continue_marker_j_empty_chain_warns_without_stopping() {
        use crate::continue_marker::CONTINUE_LOG_TARGET;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        // 空 CONTINUE を 3 連続 → 本文で終端（max=10 未達・停止しないことを示す）。
        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("CONTINUE"),
            text_response("CONTINUE"),
            text_response("CONTINUE"),
            text_response("まとめ本文"),
        ]);

        // CONTINUE_LOG_TARGET のイベントだけ数える最小 Subscriber（常時 enabled）。
        struct TargetCounter {
            hits: Arc<AtomicUsize>,
        }
        impl tracing::Subscriber for TargetCounter {
            fn enabled(&self, _md: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                if event.metadata().target() == CONTINUE_LOG_TARGET {
                    self.hits.fetch_add(1, Ordering::SeqCst);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let subscriber = TargetCounter { hits: hits.clone() };
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let response = Mutex::new(String::new());
        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let r = engine
                    .run("system", "続けて", "test-model")
                    .await
                    .expect("空 CONTINUE 連鎖は停止せず本文で終端する");
                *response.lock().unwrap() = r.response;
            });
        });

        // 停止しない: 本文まで到達し LLM を 4 回呼ぶ。
        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            4,
            "空 CONTINUE 連鎖でも停止せず本文まで進む（4 生成）"
        );
        assert_eq!(*response.lock().unwrap(), "まとめ本文");

        // 解析用 warn が CONTINUE_LOG_TARGET に 1 行以上出る（現 tip は 0 → 赤）。
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "空 CONTINUE 連続 3 回の解析 warn が CONTINUE_LOG_TARGET に出ていない（§13.1 a）"
        );
    }

