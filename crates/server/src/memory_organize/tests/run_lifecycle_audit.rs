    fn engine_result(stopped_by_limit: bool) -> EngineResult {
        EngineResult {
            response: String::new(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        }
    }

    /// ゲートが「新規を提示して発火」する状態に DB を整える（新規側だけを見る）。
    fn seed_passing_gate(state: &AppState) {
        set_watermark(state, "a1", 1000);
        set_marker(state, "a1", &hours_ago(48)); // last_organize_at（新規/過去の境界）
        set_backlog_marker(state, "a1", EPOCH); // 過去分は out（新規側だけ見る）
        set_last_run(state, "a1", &hours_ago(48)); // 日次 throttle を開ける（48h > 24h）
                                                   // 新規（境界より後）を下限（min_new）以上そろえる。順は created_at ASC で n1 → n2。
        seed_topic(state, "a1", "n1", &hours_ago(5), 50);
        seed_topic(state, "a1", "n2", &hours_ago(3), 60);
    }

    /// context="sleep" の最新監査 message を JSON で返す。
    fn latest_sleep_audit(state: &AppState) -> Option<serde_json::Value> {
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_logs(&conn, Some("a1"), None, 10).ok()?;
        rows.into_iter()
            .find(|r| r.context == "sleep")
            .and_then(|r| serde_json::from_str(&r.message).ok())
    }

    /// clean 完了: マーカーが前進し、監査に completed が残る。本番のラン構築は一切通らない。
    #[tokio::test]
    async fn clean_run_advances_markers_without_production_build() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);

        let ran = run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(ran, "ゲート通過 → 起動して true");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "ターンは 1 回だけ回る"
        );
        // clean → 新規側マーカーが提示末尾（最新 = n2）へ前進する。
        let marker = get_marker(&state, "a1").expect("新規側マーカー");
        assert!(
            marker.contains("n2"),
            "clean で新規側マーカーが提示末尾(n2)へ前進する: {marker}"
        );
        let audit = latest_sleep_audit(&state).expect("監査ログが書かれる");
        assert_eq!(audit["outcome"], "completed");
        assert_eq!(audit["marker_advanced"], true);
    }

    /// partial（ターン上限）: マーカーは据え置き、監査は stopped_by_limit。差し替えた結果だけで
    /// partial 経路を検証できる（LLM もラン構築も不要）。
    #[tokio::test]
    async fn stopped_by_limit_run_holds_markers() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let before = get_marker(&state, "a1");
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);

        let ran = run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(ran, "起動はした（partial でも true）");
        assert_eq!(
            get_marker(&state, "a1"),
            before,
            "partial（ターン上限）では新規側マーカーを進めない"
        );
        let audit = latest_sleep_audit(&state).expect("監査ログが書かれる");
        assert_eq!(audit["outcome"], "stopped_by_limit");
        assert_eq!(audit["marker_advanced"], false);
    }

    /// run 自体が Err: マーカーは据え置き、監査は error。エラー経路も差し替えで検証できる。
    #[tokio::test]
    async fn errored_run_holds_markers() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let before = get_marker(&state, "a1");
        let fake = FakeRunner::new(FakeOutcome::Error);

        let ran = run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(ran, "起動はした（error でも true）");
        assert_eq!(
            get_marker(&state, "a1"),
            before,
            "error では新規側マーカーを進めない"
        );
        let audit = latest_sleep_audit(&state).expect("監査ログが書かれる");
        assert_eq!(audit["outcome"], "error");
    }

    /// 口に渡る `RunRequest` が本番配線を保っていること（#368/#369 を壊していない）:
    /// gateway="sleep" / caller=Owner / ツール許可リスト=ORGANIZE_ALLOWED_TOOLS /
    /// 送信経路（gateway_actions）なし。
    #[tokio::test]
    async fn run_request_carries_expected_wiring() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);

        run_organize(
            &state.db,
            &cfg(true, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        let captured = fake.captured.lock().unwrap();
        let req = captured.as_ref().expect("ターンが回れば記録される");
        assert_eq!(req.gateway, "sleep", "RuntimeInfo の gateway 名");
        assert!(req.caller_is_owner, "caller は Owner");
        assert!(
            !req.has_gateway_actions,
            "送信経路（会話への出口）は渡さない"
        );
        let expected: Vec<String> = ORGANIZE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            req.tool_allowlist.as_ref(),
            Some(&expected),
            "#369 のツール許可リストがそのまま載る"
        );
        // #393: 整備作業のターンは生ログに残さない（残すと宣言ランの材料になる）。
        assert!(
            !req.persist_turn_logs,
            "整理ランのターンは memory_sessions に記録しない"
        );
    }

    /// 既定オフ: ゲートに入る前にゼロコールで返る。**口（LLM）は 1 度も呼ばれない**。
    #[tokio::test]
    async fn disabled_never_calls_the_runner() {
        let state = crate::test_app_state();
        // ゲートが通る材料を揃えても、既定オフなら口を呼ばない。
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);

        let ran = run_organize(
            &state.db,
            &cfg(false, 5, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();

        assert!(!ran, "既定オフでは起動しない");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "既定オフでは 1 ターンも回さない（LLM ゼロコール）"
        );
    }
