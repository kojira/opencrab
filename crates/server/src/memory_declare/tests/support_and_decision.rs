    use super::*;
    use opencrab_actions::Action;
    use opencrab_core::EngineResult;
    use opencrab_db::queries::DeclareWindowPref;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- セットアップ ---

    /// state の DB に生ログを 1 件入れて、その id を返す。
    fn seed_log(state: &AppState, agent_id: &str, session_id: &str, content: &str) -> i64 {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "message".to_string(),
                content: content.to_string(),
                speaker_id: None,
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap()
    }

    /// `n` 件の生ログを 1 セッションに入れ、id のリストを返す。
    fn seed_logs(state: &AppState, agent_id: &str, session_id: &str, n: usize) -> Vec<i64> {
        (0..n)
            .map(|i| seed_log(state, agent_id, session_id, &format!("発話 {i}")))
            .collect()
    }

    fn get_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_declare_cursor(&conn, agent_id).unwrap()
    }

    fn set_marker(state: &AppState, agent_id: &str, cursor: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_memory_declare_cursor(&conn, agent_id, cursor).unwrap();
    }

    fn cfg(enabled: bool, max_logs: i64, min_new: i64) -> MemoryDeclareConfig {
        MemoryDeclareConfig {
            enabled,
            max_logs,
            min_new_logs: min_new,
            min_interval_minutes: 1440,
            timeout_secs: 600,
        }
    }

    fn hours_ago(hours: i64) -> String {
        (Utc::now() - Duration::hours(hours)).to_rfc3339()
    }

    fn minutes_ago(minutes: i64) -> String {
        (Utc::now() - Duration::minutes(minutes)).to_rfc3339()
    }

    // --- マーカー parse/format ---

    #[test]
    fn marker_roundtrips_and_tolerates_missing_parts() {
        let m = format_marker("2026-08-05T00:00:00Z", 4242);
        assert_eq!(m, "2026-08-05T00:00:00Z|4242");
        assert_eq!(
            parse_marker(Some(&m)),
            (Some("2026-08-05T00:00:00Z".to_string()), 4242)
        );
        // 未実行（None）→ (None, 0)。
        assert_eq!(parse_marker(None), (None, 0));
        // `|` 無し（旧形式・素の刻時）→ 位置 0。
        assert_eq!(
            parse_marker(Some("2026-08-05T00:00:00Z")),
            (Some("2026-08-05T00:00:00Z".to_string()), 0)
        );
        // 位置がパース不能なら 0（壊れたマーカーで先頭からやり直す・落ちない）。
        assert_eq!(
            parse_marker(Some("2026-08-05T00:00:00Z|xxx")),
            (Some("2026-08-05T00:00:00Z".to_string()), 0)
        );
    }

    // --- ゲート判定（decide_declare）---

    #[test]
    fn first_run_fires_from_beginning() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 5);
        // マーカー未設定（None）: throttle 無し・cursor=0 で先頭から窓を組む。
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        match d {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.cursor_id, 0, "初回は先頭（cursor=0）");
                assert_eq!(plan.window.log_count, 3, "窓は max_logs=3 で有界");
                assert_eq!(plan.window.from_id, Some(ids[0]), "窓は最古から");
                assert_eq!(plan.window.to_id, Some(ids[2]));
                assert_eq!(plan.window.total_remaining, 5, "未宣言の総数は 5");
                // clean 前進先は窓末尾（to_id）。
                assert_eq!(plan.window.to_id, Some(ids[2]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn interval_gate_blocks_when_recent() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        // 1h 前に走った（24h 未満）→ throttle。
        set_marker(&state, "a1", &format_marker(&hours_ago(1), 0));
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("interval_not_elapsed")));
    }

    /// 間隔ゲートは**分単位**（#390）。既定 1440 分は 24 時間ゲートのまま（現行挙動を維持）で、
    /// config で分を指定するとその間隔で発火する。0 は無効化ではなく 1 分に丸める。
    #[test]
    fn interval_gate_is_minutes_with_24h_default() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        assert_eq!(
            MemoryDeclareConfig::default().min_interval_minutes,
            1440,
            "既定は 1440 分 = 24 時間（現行挙動）"
        );

        // 既定（1440 分）: 23h 前では通らない。
        let mut c = cfg(true, 3, 2);
        assert_eq!(c.min_interval_minutes, 1440);
        set_marker(&state, "a1", &format_marker(&minutes_ago(23 * 60), 0));
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Skip("interval_not_elapsed")
        ));

        // 10 分に詰めると、同じマーカーでも発火する。
        c.min_interval_minutes = 10;
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Run(_)
        ));
        // 5 分前 < 10 分 → まだ弾かれる（分の刻みが効いている）。
        set_marker(&state, "a1", &format_marker(&minutes_ago(5), 0));
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Skip("interval_not_elapsed")
        ));

        // 0 でもゲートは外れない（1 分に丸める）: 直前に走った直後は弾かれ、2 分後は通る。
        c.min_interval_minutes = 0;
        set_marker(
            &state,
            "a1",
            &format_marker(&(Utc::now() - Duration::seconds(10)).to_rfc3339(), 0),
        );
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Skip("interval_not_elapsed")
        ));
        set_marker(&state, "a1", &format_marker(&minutes_ago(2), 0));
        assert!(matches!(
            decide_declare(&state.db, &c, "a1").unwrap(),
            DeclareDecision::Run(_)
        ));
    }

    #[test]
    fn floor_gate_blocks_below_min() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 3);
        // 間隔は通る（48h 前）。未宣言は 3 件で下限 5 未満 → skip。
        set_marker(&state, "a1", &format_marker(&hours_ago(48), 0));
        let d = decide_declare(&state.db, &cfg(true, 10, 5), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("below_floor")));
    }

    #[test]
    fn empty_history_skips() {
        let state = crate::test_app_state();
        // ログ 0 件 → total_remaining 0 < 下限 → skip（発火しない）。
        let d = decide_declare(&state.db, &cfg(true, 10, 1), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("below_floor")));
    }

    #[test]
    fn window_starts_after_cursor_and_carries_survey_and_units() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 8);
        // cursor を 3 件目に置く（間隔は通る）。窓は 4 件目以降。
        set_marker(&state, "a1", &format_marker(&hours_ago(48), ids[2]));
        // 既存宣言を 1 つ作っておく（プロンプト材料に載る）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::record_memory_unit(
                &conn,
                "a1",
                "既存の宣言",
                "",
                ids[0],
                ids[1],
                None,
                None,
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        }
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        match d {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.cursor_id, ids[2]);
                assert_eq!(plan.window.from_id, Some(ids[3]), "cursor の次から");
                assert_eq!(plan.window.to_id, Some(ids[5]), "max_logs=3 で有界");
                assert_eq!(plan.window.total_remaining, 5, "cursor 以降の未宣言は 5");
                // 地図（集計）が載る。
                assert_eq!(plan.survey.total_logs, 8);
                // 既存宣言が載る（1 件）。
                assert_eq!(plan.recent_units.len(), 1);
                assert_eq!(plan.recent_units[0].title, "既存の宣言");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // --- プロンプト ---

    #[test]
    fn system_prompt_has_map_window_tools_but_no_log_bodies() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        let plan = match decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        // 地図（集計）が載る。
        assert!(sp.contains("あなたの記憶の地図"));
        assert!(sp.contains("総ログ 5 件"));
        // 今回の窓が載る。
        assert!(sp.contains("今回の範囲"));
        // 宣言の道具名が載る。
        assert!(sp.contains("record_memory_unit"));
        assert!(sp.contains("read_my_history"));
        // 生ログ本文（"発話 N"）は**渡さない**（要約を渡すと読まない / #313）。
        assert!(!sp.contains("発話 0"), "生ログ本文がプロンプトに漏れている");
        assert!(!sp.contains("発話 4"), "生ログ本文がプロンプトに漏れている");
    }

    /// #399: 本人が広さを表明済みだと「いまの設定」は自分の値しか出ない。既定を併記して
    /// 「自分の設定が既定より広いか＝自動リセットが自分に掛かるか」を本人が判定できること。
    #[test]
    fn prompt_shows_default_window_alongside_preferred() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        // 本人が既定（= max_logs = 3）より広い 100 件を表明済み。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(100),
                    ..Default::default()
                }),
            )
            .unwrap();
        }
        let plan = match decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        // 既定 = cfg.max_logs.max(1) = 3。自動リセットの `widened_beyond_default` が比べるのと同値。
        assert_eq!(plan.default_window_size, 3);
        // 表明した広さ（clamp 後 = 100）が「いまの設定」。
        assert_eq!(plan.window_size, 100);
        let sp = build_system_prompt(&plan);
        assert!(
            sp.contains("いまの設定は 100 件です（あなたが決めた広さ／既定は 3 件）"),
            "既定の併記が出ていない: {sp}"
        );
    }

    /// #399: 未表明のときは「いまの設定」がそのまま既定なので、併記は足さない（情報は最小）。
    #[test]
    fn prompt_omits_default_when_no_preference() {
        let state = crate::test_app_state();
        seed_logs(&state, "a1", "s1", 5);
        let plan = match decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(plan.preferred_window_size, None);
        let sp = build_system_prompt(&plan);
        assert!(sp.contains("（既定の広さ）"), "既定表示が無い: {sp}");
        assert!(
            !sp.contains("あなたが決めた広さ"),
            "未表明なのに表明済みの文言が出ている: {sp}"
        );
    }

    // --- 本番のラン構築を通さない全経路テスト（#370 の構造を共有）---

    enum FakeOutcome {
        Completed,
        StoppedByLimit,
        Error,
    }

    struct FakeRunner {
        outcome: FakeOutcome,
        calls: AtomicUsize,
        captured: std::sync::Mutex<Option<CapturedReq>>,
        /// ターン中に本人が `plan_next_memory_window` を呼んだ状況を模す（#394）。
        /// 道具は DB へ書くだけなので、ここで同じ列へ書けば本番と同じ経路を通る。
        writes_pref: Option<(opencrab_db::Db, DeclareWindowPref)>,
    }

    struct CapturedReq {
        gateway: String,
        caller_is_owner: bool,
        tool_allowlist: Option<Vec<String>>,
        has_gateway_actions: bool,
        persist_turn_logs: bool,
    }

    impl FakeRunner {
        fn new(outcome: FakeOutcome) -> Self {
            Self {
                outcome,
                calls: AtomicUsize::new(0),
                captured: std::sync::Mutex::new(None),
                writes_pref: None,
            }
        }

        /// ターン中に本人が窓の希望を表明する版（#394）。
        fn with_pref(mut self, state: &AppState, pref: DeclareWindowPref) -> Self {
            self.writes_pref = Some((state.db.clone(), pref));
            self
        }
    }

    #[async_trait::async_trait]
    impl OrganizeTurnRunner for FakeRunner {
        async fn run_turn(&self, req: RunRequest) -> anyhow::Result<EngineResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.captured.lock().unwrap() = Some(CapturedReq {
                gateway: req.gateway.clone(),
                caller_is_owner: matches!(req.caller, CallerIdentity::Owner),
                tool_allowlist: req.tool_allowlist.clone(),
                has_gateway_actions: req.gateway_actions.is_some(),
                persist_turn_logs: req.persist_turn_logs,
            });
            if let Some((db, pref)) = &self.writes_pref {
                let conn = db.lock().unwrap();
                opencrab_db::queries::set_memory_declare_window(&conn, "a1", Some(pref)).unwrap();
            }
            match self.outcome {
                FakeOutcome::Completed => Ok(engine_result(false)),
                FakeOutcome::StoppedByLimit => Ok(engine_result(true)),
                FakeOutcome::Error => Err(anyhow::anyhow!("simulated run failure")),
            }
        }
    }

    fn engine_result(stopped_by_limit: bool) -> EngineResult {
        EngineResult {
            response: String::new(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit,
            explicit_termination: None,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        }
    }

    /// ゲートが通る状態に DB を整える（間隔 OK / 下限以上のログ）。to_id を返す。
    fn seed_passing_gate(state: &AppState) -> i64 {
        let ids = seed_logs(state, "a1", "s1", 5);
        set_marker(state, "a1", &format_marker(&hours_ago(48), 0));
        *ids.last().unwrap()
    }

    fn latest_sleep_audit(state: &AppState) -> Option<serde_json::Value> {
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_logs(&conn, Some("a1"), None, 10).ok()?;
        rows.into_iter()
            .filter(|r| r.context == "sleep")
            .find_map(|r| {
                serde_json::from_str::<serde_json::Value>(&r.message)
                    .ok()
                    .filter(|v| v["kind"] == "memory_declare")
            })
    }

