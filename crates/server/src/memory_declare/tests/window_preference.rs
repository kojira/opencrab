    /// **自力での回復**: 本人が広げた結果ターンが毎回潰れると、位置が 1 件も進まないまま発火し
    /// 続ける（ターンが潰れる状況では本人が道具を呼ぶ余地も無い）。partial が
    /// [`MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET`] 回連続したら、広さの希望を捨てて config の
    /// 既定へ戻す。**N-1 回では戻らない**（一時的な遅延・失敗で本人の設定を消さない）。
    #[tokio::test]
    async fn consecutive_partials_reset_preferred_window_size() {
        assert_eq!(MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET, 3, "以下は N=3 前提");
        let state = crate::test_app_state();
        seed_window(&state, 400);
        // config の既定は 100。本人はそれより**広い** 300 を表明する（＝安全弁の対象）。
        let c = cfg(true, 100, 1);

        // 本人が道具で広さを表明する（実際に通る経路）。
        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 300}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);

        // 1 回目・2 回目の partial では戻さない（連続を数えるだけ）。
        for expected_streak in 1..MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET {
            let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
            run_again(&state, &c, &fake).await;
            let pref = get_pref(&state).expect("希望は残る");
            assert_eq!(
                pref.window_size,
                Some(300),
                "{expected_streak} 回目の partial で本人の設定が消えた"
            );
            assert_eq!(pref.partial_streak, Some(expected_streak));
            let audit = latest_sleep_audit(&state).unwrap();
            assert_eq!(audit["partial_streak"], json!(expected_streak));
            assert_eq!(audit["window_size_auto_reset"], json!(false));
        }

        // N 回目で既定へ戻す。
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
        run_again(&state, &c, &fake).await;
        assert_eq!(
            get_pref(&state).and_then(|p| p.window_size),
            None,
            "N 回連続の partial でも本人の広さが残っている（自力で回復できない）"
        );
        assert_eq!(get_pref(&state).and_then(|p| p.partial_streak), None);
        let audit = latest_sleep_audit(&state).unwrap();
        assert_eq!(
            audit["window_size_auto_reset"],
            json!(true),
            "自動で戻したことが監査から分からない"
        );

        // 次のランは config の既定の広さで走る（throttle だけ開ける）。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window_size, 100, "config の既定へ戻っていない");
                assert_eq!(plan.preferred_window_size, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }

        // 恒久的な禁止ではない: 本人が呼べばまた広げられる。
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 250}), &ctx)
            .await;
        assert!(r.success);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window_size, 250),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// **狭める方向の表明は巻き添えにしない**（#394 のオーナー要件「密に拾う個性 → 濃い範囲では
    /// 窓を縮めて丁寧に見たい」）。
    ///
    /// 既定より狭い設定は partial の原因になり得ない（timeout / ターン上限は広い窓の側で起きる）。
    /// ここで破棄すると、窓は既定へ**広がって**状況を悪化させる方向へ動く。`clean` は
    /// `completed` だけが真で LLM 側の一時障害も 1 回として数えるので、消化中
    /// （`min_interval_minutes = 1`）はプロバイダの不調だけで連続が伸びる——現実に踏む。
    #[tokio::test]
    async fn narrower_than_default_preference_is_never_auto_reset() {
        let state = crate::test_app_state();
        seed_window(&state, 400);
        let c = cfg(true, 100, 1); // 既定 100 に対して本人は 60（狭める方向）

        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": 60}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);

        // N を超えて partial が続いても破棄しない。連続も数えない。
        for _ in 0..MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET + 2 {
            let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
            run_again(&state, &c, &fake).await;
            let pref = get_pref(&state).expect("希望は残る");
            assert_eq!(
                pref.window_size,
                Some(60),
                "狭める方向の設定を機械が取り上げた（窓が既定へ広がってしまう）"
            );
            assert_eq!(pref.partial_streak, None, "対象外なのに連続を数えている");
            assert_eq!(
                latest_sleep_audit(&state).unwrap()["window_size_auto_reset"],
                json!(false)
            );
        }
        // 次のランも本人の 60 のまま。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window_size, 60),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// **本人の自己修正は巻き添えにしない**。希望はターンの**後**に読むので、連続が N-1 まで
    /// 来た状態で本人がターン中に「広すぎたので狭くする」と表明し、そのターンも partial に
    /// 落ちても、**いま書いたばかりの狭い値ごと**破棄されてはいけない。
    #[tokio::test]
    async fn self_correction_during_the_turn_is_not_swept_away() {
        let state = crate::test_app_state();
        seed_window(&state, 400);
        let c = cfg(true, 100, 1);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(300),
                    // 既に N-1 回連続している状態から始める。
                    partial_streak: Some(MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET - 1),
                    ..Default::default()
                }),
            )
            .unwrap();
        }

        // このターンの中で本人が既定以下へ狭め、ターン自体は partial に終わる。
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit).with_pref(
            &state,
            DeclareWindowPref {
                window_size: Some(80),
                partial_streak: Some(MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET - 1),
                ..Default::default()
            },
        );
        run_again(&state, &c, &fake).await;

        let pref = get_pref(&state).expect("希望は残る");
        assert_eq!(
            pref.window_size,
            Some(80),
            "本人が書いたばかりの狭い値が N 回目として破棄された"
        );
        assert_eq!(
            pref.partial_streak, None,
            "既定以下になったので連続は切れる"
        );
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["window_size_auto_reset"],
            json!(false)
        );
    }

    /// clean が 1 回通れば連続は切れる（間に成功が挟まれば本人の設定は消えない）。
    #[tokio::test]
    async fn clean_run_breaks_the_partial_streak() {
        let state = crate::test_app_state();
        seed_window(&state, 800);
        // 既定 100 より広い 300（＝安全弁の対象）でないと、そもそも連続を数えない。
        let c = cfg(true, 100, 1);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(300),
                    ..Default::default()
                }),
            )
            .unwrap();
        }

        // partial × (N-1) → clean → partial × (N-1)。どこにも N 連続は無い。
        for outcome in [
            FakeOutcome::StoppedByLimit,
            FakeOutcome::Error,
            FakeOutcome::Completed,
            FakeOutcome::StoppedByLimit,
            FakeOutcome::Error,
        ] {
            let fake = FakeRunner::new(outcome);
            run_again(&state, &c, &fake).await;
        }
        assert_eq!(
            get_pref(&state).and_then(|p| p.window_size),
            Some(300),
            "clean を挟んでいるのに本人の設定が消えた"
        );
        assert_eq!(
            get_pref(&state).and_then(|p| p.partial_streak),
            Some(2),
            "clean 後の連続だけが数えられているはず"
        );
    }

    /// 広さを表明していないエージェントでは連続を数えない（戻す先が無い＝仕事が無い）。
    /// 希望の行を作らないので、道具を一度も使っていない DB は NULL のまま。
    #[tokio::test]
    async fn partials_without_a_preference_do_not_create_state() {
        let state = crate::test_app_state();
        seed_window(&state, 200);
        let c = cfg(true, 100, 1);
        for _ in 0..MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET + 1 {
            let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
            run_again(&state, &c, &fake).await;
        }
        assert_eq!(get_pref(&state), None, "希望なしの行を作ってはいけない");
        assert_eq!(
            latest_sleep_audit(&state).unwrap()["partial_streak"],
            json!(0)
        );
    }

    /// **窓の広さ**: 本人の表明が次の窓に効き、上下限へ丸められる。表明が無ければ config の既定
    /// のまま（既定値は変えない）。
    #[test]
    fn preferred_window_size_resizes_next_window_within_bounds() {
        let state = crate::test_app_state();
        seed_window(&state, 200);
        let c = cfg(true, 100, 1);

        // 表明なし: config の既定（100）。既定が下限 50 を下回る設定でも丸めない。
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window_size, 100);
                assert_eq!(plan.window.log_count, 100);
                assert_eq!(plan.preferred_window_size, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }
        match decide_declare(&state.db, &cfg(true, 20, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size, 20,
                "表明が無ければ config の既定をそのまま使う（既定値を変えない）"
            ),
            other => panic!("expected Run, got {other:?}"),
        }

        // 広げる（薄かったので次はもっと広く）。
        let set = |size: i64| {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_memory_declare_window(
                &conn,
                "a1",
                Some(&DeclareWindowPref {
                    window_size: Some(size),
                    ..Default::default()
                }),
            )
            .unwrap();
        };
        set(150);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window_size, 150);
                assert_eq!(plan.window.log_count, 150, "実際の窓が広がる");
                assert_eq!(plan.preferred_window_size, Some(150));
            }
            other => panic!("expected Run, got {other:?}"),
        }

        // 狭める。
        set(60);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.window.log_count, 60),
            other => panic!("expected Run, got {other:?}"),
        }

        // 上限・下限で丸める（プロンプトが肥大しない / 前進が止まらない）。
        set(100_000);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size,
                opencrab_actions::memory_units::DECLARE_WINDOW_MAX
            ),
            other => panic!("expected Run, got {other:?}"),
        }
        set(1);
        match decide_declare(&state.db, &c, "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size,
                opencrab_actions::memory_units::DECLARE_WINDOW_MIN
            ),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 運用が `max_logs` を上限より大きく設定している場合、本人の表明でそれより狭められることは
    /// あっても、上限のせいで運用の設定より狭くなることは無い。
    ///
    /// **道具（`plan_next_memory_window`）経由で**表明する——本人が実際に通る経路。DB を直接
    /// 叩くと、道具が上限で丸めてしまう実装でもこのテストは通ってしまい、doc の約束
    /// （`DECLARE_WINDOW_MAX` の doc）との食い違いを検出できない。
    #[tokio::test]
    async fn preferred_window_size_ceiling_never_undercuts_config_via_tool() {
        let state = crate::test_app_state();
        seed_window(&state, 60);
        let big = opencrab_actions::memory_units::DECLARE_WINDOW_MAX + 400;

        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(&json!({"window_size": big}), &ctx)
            .await;
        assert!(r.success, "{:?}", r.error);

        // 運用が上限より広い枠を既定にしている: 本人が同じ値を表明しても窓は狭まらない。
        match decide_declare(&state.db, &cfg(true, big, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size, big,
                "本人が表明した瞬間に窓が運用の既定より狭まってはいけない"
            ),
            other => panic!("expected Run, got {other:?}"),
        }
        // 運用が既定（上限より狭い）なら、同じ表明が上限で丸められる。
        match decide_declare(&state.db, &cfg(true, 100, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(
                plan.window_size,
                opencrab_actions::memory_units::DECLARE_WINDOW_MAX
            ),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 道具経由で位置と広さを表明し、**ラン → 次の窓**まで通す（本人が実際に通る経路の一気通貫）。
    #[tokio::test]
    async fn tool_expressed_window_flows_through_a_real_run() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 200);
        let (_dir, ctx) = declare_tool_ctx(&state);
        let r = opencrab_actions::memory_units::PlanNextMemoryWindowAction
            .execute(
                &json!({"next_from_id": ids[40], "window_size": 80, "note": "まだ続いている"}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        // 窓 60 のランが clean で終わると、カーソルは道具で指した 1 つ手前へ。
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 60, 1),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(cursor_of(&state), ids[39]);
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["window_note"], json!("まだ続いている"));

        // 次のランの窓は、道具で表明した広さ 80（config の 60 ではない）で組まれる。
        set_marker(
            &state,
            "a1",
            &format_marker(&hours_ago(48), cursor_of(&state)),
        );
        match decide_declare(&state.db, &cfg(true, 60, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window.from_id, Some(ids[40]), "指した id から再開する");
                assert_eq!(plan.window_size, 80);
                assert_eq!(plan.window.log_count, 80);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 丸めの下限・上限は**窓と同じ時点**で決まり、生ログの件数で測られる。
    #[test]
    fn position_bounds_are_measured_in_rows() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 40);
        match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.min_position, ids[2], "窓 9 件の 1/3 = 3 件目");
                assert_eq!(plan.max_position, ids[17], "窓 9 件の 2 倍 = 18 件目");
                assert!(plan.min_position <= plan.window.to_id.unwrap());
                assert!(plan.max_position >= plan.window.to_id.unwrap());
            }
            other => panic!("expected Run, got {other:?}"),
        }
        // 生ログが 2 窓ぶんに満たないときは、上限は「あるだけ」（最後の id）。
        let state2 = crate::test_app_state();
        let ids2 = seed_window(&state2, 12);
        match decide_declare(&state2.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(plan) => assert_eq!(plan.max_position, ids2[11]),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// プロンプトに「窓は自分で調整できる」ことが**書いてある**（道具を足しても説明が無ければ
    /// 使われない）。今回の広さと、丸めの範囲も示す。
    #[test]
    fn system_prompt_explains_window_control() {
        let state = crate::test_app_state();
        let ids = seed_window(&state, 9);
        let plan = match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        assert!(sp.contains("plan_next_memory_window"), "道具名が無い");
        assert!(
            sp.contains("範囲の切り方もあなたが決められます"),
            "説明が無い"
        );
        assert!(sp.contains("next_from_id"), "持ち越しの指定方法が無い");
        assert!(sp.contains("window_size"), "広さの変え方が無い");
        // 今回の広さ（9 件）と既定/本人の別。
        assert!(sp.contains("いまの設定は 9 件です（既定の広さ）"));
        // 丸めの範囲は next_from_id として指せる値（＝位置 + 1）で示す。
        assert!(sp.contains(&format!("id {} 〜 {}", ids[2] + 1, ids[8] + 1)));
    }

    /// **約束と実装が一致していること**: 広さは sticky だが、機械が既定へ戻すことがある。
    /// プロンプトはその条件（既定より広い / N 回連続）まで書き、狭めた設定は戻らないと言う。
    ///
    /// 回数は**実装の定数から組む**ので、`MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET` を変えれば
    /// 文面も追随する（片方だけ変わって食い違うことがない）。
    #[test]
    fn system_prompt_promise_matches_the_auto_reset_rule() {
        let state = crate::test_app_state();
        seed_window(&state, 9);
        let plan = match decide_declare(&state.db, &cfg(true, 9, 1), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let sp = build_system_prompt(&plan);
        // sticky であることは引き続き言う。
        assert!(sp.contains("一度決めると変えるまで効き続けます"));
        // ただし機械が戻すことがある、という但し書きが同じ場所にある。
        assert!(
            sp.contains(&format!(
                "{MAX_PARTIAL_STREAK_BEFORE_WINDOW_RESET} 回続いたら"
            )),
            "自動で戻す条件（回数）が実装の定数と結びついていない: {sp}"
        );
        assert!(
            sp.contains("既定の広さへ自動で戻します"),
            "自動で戻すことが書かれていない"
        );
        // 戻す対象は「既定より広げた設定」だけ、という条件まで書く（狭めた設定は戻らない）。
        assert!(sp.contains("既定より広げた設定"), "対象の条件が無い");
        assert!(
            sp.contains("狭めた設定はそのままです"),
            "対象外の明示が無い"
        );
    }

    /// 残ログが窓より少ないとき、プロンプトの 2 つの件数（提示した実数と設定値）が
    /// **矛盾して読めない**こと。設定 100 / 残り 9 件で「9 件」と「100 件」が並ぶ形。
    #[test]
    fn system_prompt_distinguishes_actual_range_from_configured_size() {
        let state = crate::test_app_state();
        seed_window(&state, 9);
        let plan = match decide_declare(&state.db, &cfg(true, 100, 1), "a1").unwrap() {
            DeclareDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(plan.window.log_count, 9, "実際に提示できるのは 9 件");
        assert_eq!(plan.window_size, 100, "設定は 100 件");
        let sp = build_system_prompt(&plan);
        // 提示した範囲は実数で書く。
        assert!(sp.contains("今回の範囲（未宣言"));
        assert!(sp.contains("/ 9 件 /"));
        // 広さは「設定」として書き分け、実数がこれより少なくなり得ることを添える。
        assert!(sp.contains("いまの設定は 100 件です"));
        assert!(
            sp.contains("これより少なくなります"),
            "設定値と実数がずれ得ることの説明が無い"
        );
        // 「今回は 100 件」という、提示した実数と読める書き方をしていない。
        assert!(!sp.contains("今回は 100 件"));
    }
