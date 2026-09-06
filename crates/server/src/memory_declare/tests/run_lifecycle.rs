    #[tokio::test]
    async fn default_off_is_zero_call_and_writes_nothing() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        // 既定オフ用にマーカーを消す（seed_passing_gate が立てるので上書きで None にはできない;
        // 代わりに enabled=false で decide に入らないことを確認する）。
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let before = get_marker(&state, "a1");
        let ran = run_declare(
            &state.db,
            &cfg(false, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(!ran, "既定オフでは起動しない");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "口を 1 度も呼ばない");
        assert_eq!(
            get_marker(&state, "a1"),
            before,
            "既定オフではマーカーを書き換えない"
        );
    }

    #[tokio::test]
    async fn clean_run_advances_marker() {
        let state = crate::test_app_state();
        let to_id = seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        let ran = run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "ターンは 1 回");
        // clean → 位置が提示窓の末尾（3 件目）へ進む。max_logs=3 なので to は ids[2]。
        let marker = get_marker(&state, "a1").expect("マーカーが立つ");
        let (_, cursor) = parse_marker(Some(&marker));
        assert_eq!(cursor, to_id - 2, "窓末尾（3 件目）へ前進");
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["outcome"], "completed");
        assert_eq!(audit["position_advanced"], true);
        assert_eq!(audit["throttle_advanced"], true);
    }

    /// partial（ターン上限）: **位置は据え置き・throttle は now へ前進**。その結果、次 tick は
    /// 日次ゲートで弾かれ、同じ窓で再発火しない（#366 と同型の暴走防止 / 無人の連続失敗を止める）。
    #[tokio::test]
    async fn partial_holds_position_but_advances_throttle() {
        let state = crate::test_app_state();
        seed_passing_gate(&state); // marker = "{48h前}|0"
        let (before_run, before_pos) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(before_pos, 0);
        let fake = FakeRunner::new(FakeOutcome::StoppedByLimit);
        let ran = run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        assert!(ran, "起動はした（partial でも true）");
        let (after_run, after_pos) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(after_pos, 0, "partial では位置を進めない（据え置き）");
        // throttle は now へ進んだ（48h 前より新しい）。
        let before_dt = before_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        let after_dt = after_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        assert!(after_dt > before_dt, "partial でも throttle は now へ進む");
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["outcome"], "stopped_by_limit");
        assert_eq!(audit["position_advanced"], false);
        assert_eq!(audit["throttle_advanced"], true);
        // 次 tick は日次ゲートで弾かれる（10 分後の再発火を止める）。
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(
            matches!(d, DeclareDecision::Skip("interval_not_elapsed")),
            "partial 直後は throttle で弾かれ再発火しない"
        );
    }

    /// error（run 自体の失敗）も partial と同じ: 位置据え置き・throttle 前進・次 tick はゲート。
    #[tokio::test]
    async fn error_holds_position_but_advances_throttle() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let (before_run, _) = parse_marker(get_marker(&state, "a1").as_deref());
        let fake = FakeRunner::new(FakeOutcome::Error);
        run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        let (after_run, after_pos) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(after_pos, 0, "error では位置を進めない");
        let before_dt = before_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        let after_dt = after_run.unwrap().parse::<DateTime<Utc>>().unwrap();
        assert!(after_dt > before_dt, "error でも throttle は now へ進む");
        let audit = latest_sleep_audit(&state).expect("監査ログ");
        assert_eq!(audit["outcome"], "error");
        assert_eq!(audit["position_advanced"], false);
        // 次 tick はゲートで弾かれる。
        let d = decide_declare(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, DeclareDecision::Skip("interval_not_elapsed")));
    }

    /// マーカーが前進し、次回は次の窓を提示する（提示済みを二度出さない = 無限ループしない）。
    #[tokio::test]
    async fn marker_progresses_across_runs_without_repeat() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 6);
        set_marker(&state, "a1", &format_marker(&hours_ago(48), 0));

        // run1: 先頭 3 件（ids[0..3]）を提示 → clean で cursor=ids[2]。
        let fake1 = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 3, 1),
            &state.index_build_inflight,
            "a1",
            &fake1,
        )
        .await
        .unwrap();
        let (_, c1) = parse_marker(get_marker(&state, "a1").as_deref());
        assert_eq!(c1, ids[2]);

        // 翌日を模す（throttle を開ける）。位置はそのまま。
        set_marker(&state, "a1", &format_marker(&hours_ago(48), ids[2]));

        // run2: 次の窓（ids[3..6]）を提示することを decide で確認。
        let d = decide_declare(&state.db, &cfg(true, 3, 1), "a1").unwrap();
        match d {
            DeclareDecision::Run(plan) => {
                assert_eq!(plan.window.from_id, Some(ids[3]), "提示済みを跨いで次から");
                assert_eq!(plan.window.to_id, Some(ids[5]));
                assert_eq!(plan.window.total_remaining, 3);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 口に渡る `RunRequest` が本番配線を保つ: gateway="sleep" / caller=Owner /
    /// ツール許可リスト=DECLARE_ALLOWED_TOOLS / 送信経路（gateway_actions）なし。
    #[tokio::test]
    async fn run_request_carries_expected_wiring() {
        let state = crate::test_app_state();
        seed_passing_gate(&state);
        let fake = FakeRunner::new(FakeOutcome::Completed);
        run_declare(
            &state.db,
            &cfg(true, 3, 2),
            &state.index_build_inflight,
            "a1",
            &fake,
        )
        .await
        .unwrap();
        let captured = fake.captured.lock().unwrap();
        let req = captured.as_ref().expect("ターンが回れば記録される");
        assert_eq!(req.gateway, "sleep");
        assert!(req.caller_is_owner, "caller は Owner");
        assert!(
            !req.has_gateway_actions,
            "送信経路（会話への出口）は渡さない"
        );
        let expected: Vec<String> = DECLARE_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(req.tool_allowlist.as_ref(), Some(&expected));
        // #393: 整備作業のターンは生ログに残さない（残すと次の宣言ランの材料になる）。
        assert!(
            !req.persist_turn_logs,
            "宣言ランのターンは memory_sessions に記録しない"
        );
    }

    /// 許可リストの内容（経路1）: 宣言に要る道具は入り、外向き・タグ整理の道具は入らない。
    #[test]
    fn declare_allowlist_includes_record_excludes_outward_and_tag_tools() {
        let allowed = [
            "survey_my_history",
            "read_my_history",
            "search_my_history",
            "record_memory_unit",
            "retract_memory_unit",
            "declare_done",
        ];
        for a in allowed {
            assert!(
                DECLARE_ALLOWED_TOOLS.contains(&a),
                "宣言に必要な {a} が許可リストに無い"
            );
        }
        // 外向き・状態書き換え・タグ整理（別ラン）の道具は入っていない。
        let forbidden = [
            "execute_shell",
            "nostr_run",
            "spawn_subtask",
            "ws_write",
            "ws_delete",
            "configure_self",
            "update_instructions",
            "tag_topic",
            "untag_topic",
            "merge_tags",
            "browse_memory_index",
            "search_memory_index",
            "retrieve_memory_nodes",
        ];
        for f in forbidden {
            assert!(
                !DECLARE_ALLOWED_TOOLS.contains(&f),
                "許可リストに入ってはならない {f} が入っている"
            );
        }
    }

    /// 回帰（#379/#383 の構造的分離を段階2 でも固定）: 宣言ユニット（node_type='unit'）は
    /// タグ整理ランの worklist（node_type='topic' / source_type='session_log' を pin）に混ざらない。
    #[test]
    fn declared_units_do_not_mix_into_tag_worklist() {
        let state = crate::test_app_state();
        let ids = seed_logs(&state, "a1", "s1", 4);
        let conn = state.db.lock().unwrap();
        // 生ログ由来の topic を 2 件（タグ整理ランの worklist 対象）。
        for (i, end) in [ids[1], ids[3]].into_iter().enumerate() {
            opencrab_db::queries::insert_index_node(
                &conn,
                &IndexNodeRow {
                    id: format!("topic-{i}"),
                    agent_id: "a1".to_string(),
                    parent_id: None,
                    node_type: "topic".to_string(),
                    source_type: "session_log".to_string(),
                    title: format!("topic {i}"),
                    summary: "s".to_string(),
                    start_log_id: None,
                    end_log_id: Some(end),
                    source_session_id: None,
                    date_from: None,
                    date_to: None,
                    depth: 3,
                    child_count: 0,
                    token_count: 0,
                    created_at: "2026-08-01T00:00:00Z".to_string(),
                    updated_at: "2026-08-01T00:00:00Z".to_string(),
                    short_id: Some(format!("t{i}")),
                    keywords_json: "[]".to_string(),
                    summary_refreshed_at: None,
                },
            )
            .unwrap();
        }
        // 宣言ユニットを 1 件（node_type='unit'）。
        opencrab_db::queries::record_memory_unit(
            &conn,
            "a1",
            "宣言",
            "",
            ids[0],
            ids[3],
            None,
            None,
            "2026-08-02T00:00:00Z",
        )
        .unwrap();

        // タグ整理ランの発火下限クエリは topic だけを数える（unit は混ざらない）。
        let cursor = Some(("1970-01-01T00:00:00Z", ""));
        let n =
            opencrab_db::queries::count_organize_topics(&conn, "a1", cursor, 1_000_000).unwrap();
        assert_eq!(
            n, 2,
            "worklist に宣言ユニットが混ざった（topic 2 件のはず）"
        );
        let worklist =
            opencrab_db::queries::list_organize_topics(&conn, "a1", cursor, 1_000_000, 50).unwrap();
        assert!(
            worklist.iter().all(|t| t.node_type == "topic"),
            "worklist に node_type='unit' が現れた"
        );
        // 一方 list_memory_units には宣言ユニットが 1 件だけ出る。
        let units = opencrab_db::queries::list_memory_units(&conn, "a1").unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].node_type, "unit");
    }

    // ---- #394: 窓の境界と広さを本人が決める（前進の保証つき）----

