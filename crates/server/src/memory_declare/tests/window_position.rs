    fn cursor_of(state: &AppState) -> i64 {
        parse_marker(get_marker(state, "a1").as_deref()).1
    }

    fn get_pref(state: &AppState) -> Option<DeclareWindowPref> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_declare_window(&conn, "a1").unwrap()
    }

    /// 位置の希望だけを持つ `DeclareWindowPref`。
    fn want_next_from(id: i64) -> DeclareWindowPref {
        DeclareWindowPref {
            next_from_id: Some(id),
            ..Default::default()
        }
    }

    /// 宣言ランの中で道具を呼ぶときと同じ `ActionContext`（caller=Owner / gateway="sleep" /
    /// 同じ DB）。窓の道具は DB しか触らないので、これで本番と同じ経路を通せる。
    fn declare_tool_ctx(state: &AppState) -> (tempfile::TempDir, opencrab_actions::ActionContext) {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = opencrab_actions::ActionContext {
            caller: CallerIdentity::Owner,
            agent_id: "a1".to_string(),
            agent_name: "a1".to_string(),
            session_id: Some("sleep-declare-a1-1".to_string()),
            db: state.db.clone(),
            workspace: std::sync::Arc::new(workspace),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(
                opencrab_actions::RuntimeInfo {
                    default_model: "mock:test".to_string(),
                    active_model: None,
                    available_providers: vec!["mock".to_string()],
                    gateway: "sleep".to_string(),
                },
            )),
        };
        (dir, ctx)
    }

    /// throttle だけ開けて（位置はそのまま）もう 1 ラン回す。翌日 / 次の tick を模す。
    async fn run_again(state: &AppState, c: &MemoryDeclareConfig, fake: &FakeRunner) {
        set_marker(
            state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(state)),
        );
        run_declare(&state.db, c, &state.index_build_inflight, "a1", fake)
            .await
            .unwrap();
    }

    /// ゲートが通る状態に `n` 件の生ログを積む（cursor=0 / 間隔 OK）。id を返す。
    fn seed_window(state: &AppState, n: usize) -> Vec<i64> {
        let ids = seed_logs(state, "a1", "s1", n);
        set_marker(state, "a1", &format_marker(&hours_ago(48), 0));
        ids
    }

    /// **前進の保証(1)**: 何も表明しない（宣言ゼロ相当）ラン。従来どおり窓の終端へ進む。
    #[tokio::test]
    async fn no_request_advances_to_window_end() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[8], "希望が無ければ窓の終端へ");
    }

    /// **前進の保証(2)**: 本人が現在位置以下（＝巻き戻し）を指定しても、必ず前へ進む。
    /// これを落とすと同じ窓を永久に再取得するループに入る（#374）。
    #[tokio::test]
    async fn request_at_or_below_cursor_still_advances() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        // cursor は 0。「次は id 1 から」＝ 1 件も進めない要求。
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[0]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 下限（提示窓 9 件の 1/3 = 3 件目）まで引き上げられる。
        assert_eq!(cursor_of(&state), ids[2], "下限まで必ず前進する");
        assert!(cursor_of(&state) > 0, "前進していない（無限ループの入口）");

        // 次のランは進んだ先から始まる（同じ窓を再取得しない）。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window.from_id, Some(ids[3])),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// **前進の保証(3)**: 0 や負の指定（モデルが空値で埋めた形）でも下限まで進む。
    #[tokio::test]
    async fn nonsense_request_still_advances() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let fake = FakeRunner::new(FakeOutcome::Completed).with_pref(
            &state,
            DeclareWindowPref {
                next_from_id: Some(-42),
                ..Default::default()
            },
        );
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[2], "壊れた指定でも下限まで前進する");
    }

    /// 本来の用途: **続いている出来事の末尾を次回へ回す**。窓の途中を指せばそこから次回に現れる。
    #[tokio::test]
    async fn request_inside_window_rolls_the_tail_over() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        // 「id ids[5] から先はまだ続いているので次回に回したい」
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[5]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[4], "指した id の 1 つ手前で止まる");

        // 翌日: 回した末尾（ids[5..]）がちゃんともう一度現れる（従来は二度と現れなかった）。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window.from_id, Some(ids[5]), "末尾が次の窓に戻る");
                assert_eq!(plan.window.to_id, Some(ids[8]));
            }
            other => panic!("expected Run, got {other:?}"),
        }
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["requested_next_from_id"], json!(ids[5]));
        assert_eq!(audit["position"], json!(ids[4]));
    }

    /// **上限**: 窓の終端を大きく越える指定でも、2 窓ぶんより先へは飛ばない（未読を丸ごと
    /// 飛ばして二度と窓に入らないのを防ぐ）。
    #[tokio::test]
    async fn request_far_beyond_window_is_capped() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 40);
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(999_999));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 窓は 9 件。上限は 2 窓ぶん = 18 件目。
        assert_eq!(cursor_of(&state), ids[17], "上限（2 窓ぶん）で止まる");
        // 残りは失われていない。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window.total_remaining, 22),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 窓の終端を**少しだけ**越えた指定（越境して宣言した続きから）はそのまま通る。
    #[tokio::test]
    async fn request_just_past_window_end_is_honored() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 20);
        // 窓は 9 件（ids[0..9]）。本人は ids[10] まで宣言したので「次は ids[11] から」。
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[11]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[10], "越境した宣言のぶんは重複しない");
    }

    /// **partial では据え置き**（既存の挙動を壊さない）。位置の希望はランで使い切って消える。
    #[tokio::test]
    async fn partial_holds_position_even_with_request_and_consumes_it() {
        let state = crate::test_app_state();
        seed_window(&state, 9);
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit).with_pref(
            &state,
            DeclareWindowPref {
                next_from_id: Some(5),
                window_size: Some(120),
                ..Default::default()
            },
        );
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), 0, "partial では位置を進めない");
        let pref = get_pref(&state).expect("希望の行は残る");
        assert_eq!(pref.next_from_id, None, "位置の希望はランで使い切る");
        assert_eq!(pref.window_size, Some(120), "広さは残る（sticky）");
    }

    /// 位置の希望は clean でも使い切る（過去の指定が後のランを引き戻し続けない）。
    #[tokio::test]
    async fn position_request_is_consumed_after_clean_run() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let fake =
            FakeRunner::new(FakeOutcome::Completed).with_pref(&state, want_next_from(ids[5]));
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // want_next_from は updated_at を書かないので、位置を使い切ると `after` は
        // `Default::default()` と一致し、列ごと NULL へ戻る（run_declare の
        // `after != Default::default()` 判定）。
        //
        // 注意（#399）: 全 NULL に戻るのは「位置だけ・updated_at 無し」のこの経路だけ。
        // 道具（`plan_next_memory_window` / memory_units.rs）は必ず `updated_at = Some(now)`
        // を書くため、同じく位置を使い切っても列には `{"updated_at":…}` が残り、get_pref は
        // `Some` を返す。ただし `window_size` が `None` なら既定の広さで走る点は等価で実害は
        // 無い。この assert が固定しているのは「位置の希望はランで使い切る」ことだけ。
        assert_eq!(get_pref(&state).and_then(|p| p.next_from_id), None);
        assert_eq!(
            get_pref(&state),
            None,
            "位置だけの希望（updated_at 無し）は使い切ると列ごと NULL へ戻る"
        );

        // 2 回目（希望なし）は窓の終端まで進む＝古い指定が生き残っていない。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        let fake2 = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake2,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[8]);
    }

    /// `note` は位置と**同じ寿命**で消費される。残すと、以後すべてのランの監査 `window_note` に
    /// 同じ文字列が出続け、「このランで本人がこう書いた」と誤読される。
    #[tokio::test]
    async fn note_is_consumed_together_with_the_position() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 20);
        let fake = FakeRunner::new(FakeOutcome::Completed).with_pref(
            &state,
            DeclareWindowPref {
                next_from_id: Some(ids[5]),
                note: Some("この出来事はまだ続いている".to_string()),
                ..Default::default()
            },
        );
        run_declare(
            &state.db,
            &cfg(true, 9, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        // 書かれたランの監査には出る。
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["window_note"],
            json!("この出来事はまだ続いている")
        );
        assert_eq!(
            get_pref(&state).and_then(|p| p.note),
            None,
            "note は残らない"
        );

        // 次のラン（本人は何も書いていない）の監査には出ない。
        let fake2 = FakeRunner::new(FakeOutcome::Completed);
        run_again(&state, &cfg(true, 9, 1), &fake2).await;
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["window_note"],
            json!(null),
            "過去のランの note が後のランの監査に出続けている"
        );
    }

