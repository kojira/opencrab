    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- ゲート判定（decide_organize）用のセットアップ ---

    fn cfg(enabled: bool, max_topics: i64, min_new: i64) -> MemoryOrganizeConfig {
        MemoryOrganizeConfig {
            enabled,
            max_topics,
            min_new_topics: min_new,
            min_interval_minutes: 1440,
            timeout_secs: 600,
        }
    }

    /// state の DB に watermark を刻む（スナップショット上端）。
    fn set_watermark(state: &AppState, agent_id: &str, last_log_id: i64) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_index_watermark(
            &conn,
            &opencrab_db::queries::WatermarkRow {
                agent_id: agent_id.to_string(),
                last_indexed_log_id: last_log_id,
                last_indexed_at: "2026-08-03T00:00:00Z".to_string(),
                total_nodes: 0,
            },
        )
        .unwrap();
    }

    /// state の DB に topic を 1 件入れる。
    fn seed_topic(state: &AppState, agent_id: &str, id: &str, created_at: &str, end_log_id: i64) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: agent_id.to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("題 {id}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: Some(end_log_id),
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 3,
                child_count: 0,
                token_count: 0,
                created_at: created_at.to_string(),
                updated_at: created_at.to_string(),
                short_id: Some(id.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    fn set_marker(state: &AppState, agent_id: &str, ts: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_last_organize_at(&conn, agent_id, ts).unwrap();
    }

    fn get_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_last_organize_at(&conn, agent_id).unwrap()
    }

    fn set_backlog_marker(state: &AppState, agent_id: &str, cursor: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_organize_backlog_cursor(&conn, agent_id, cursor).unwrap();
    }

    fn get_backlog_marker(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_organize_backlog_cursor(&conn, agent_id).unwrap()
    }

    /// throttle 用刻時（`organize_last_run_at`）を刻む。日次ゲートを開け閉めするテストで使う。
    fn set_last_run(state: &AppState, agent_id: &str, ts: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_organize_last_run_at(&conn, agent_id, ts).unwrap();
    }

    fn get_last_run(state: &AppState, agent_id: &str) -> Option<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_organize_last_run_at(&conn, agent_id).unwrap()
    }

    /// 遡り側マーカーを最古（epoch）に置く。過去分（`created_at < epoch`）は 0 件になるので、
    /// 新規側だけを見たいゲート/worklist テストで遡りの影響を消せる。
    const EPOCH: &str = "1970-01-01T00:00:00Z";
    fn disable_backlog(state: &AppState, agent_id: &str) {
        set_backlog_marker(state, agent_id, EPOCH);
    }

    /// 現在から `hours` 時間前の rfc3339。
    fn hours_ago(hours: i64) -> String {
        (Utc::now() - Duration::hours(hours)).to_rfc3339()
    }

    /// 現在から `minutes` 分前の rfc3339。
    fn minutes_ago(minutes: i64) -> String {
        (Utc::now() - Duration::minutes(minutes)).to_rfc3339()
    }

    #[tokio::test]
    async fn default_off_is_zero_call_and_writes_nothing() {
        let mut state = crate::test_app_state();
        state.memory_organize = cfg(false, 3, 2);
        // ゲートが通る材料を揃えても、既定オフなら decide にすら入らない。
        set_watermark(&state, "a1", 1000);
        for i in 0..5 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(1), 10 + i);
        }
        let ran = maybe_run_memory_organize(&state, "a1").await.unwrap();
        assert!(!ran, "既定オフでは起動しない");
        // decide に入っていれば初回シードでマーカーが立つはず。立っていない＝ゼロコールの証跡。
        assert_eq!(get_marker(&state, "a1"), None, "既定オフでは DB を書かない");
    }

    #[test]
    fn first_encounter_seeds_marker_and_skips() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        for i in 0..5 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(1), 10 + i);
        }
        // マーカー未設定（None）。初回遭遇は両軸を now にシードしてスキップ（既存を一気に対象化しない）。
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, OrganizeDecision::Seeded));
        assert!(
            get_marker(&state, "a1").is_some(),
            "初回で新規側マーカーがシードされる"
        );
        assert!(
            get_backlog_marker(&state, "a1").is_some(),
            "初回で遡り側マーカーもシードされる（2軸）"
        );
        assert!(
            get_last_run(&state, "a1").is_some(),
            "初回で throttle 刻時もシードされる"
        );
    }

    #[test]
    fn interval_gate_blocks_when_recent() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        for i in 0..5 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(1), 10 + i);
        }
        set_marker(&state, "a1", &hours_ago(1)); // 1h 前 = 24h 未満
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(d, OrganizeDecision::Skip("interval_not_elapsed")));
    }

    /// 間隔ゲートは**分単位**（#390）。既定 1440 分は 24 時間ゲートのまま（現行挙動を維持）で、
    /// config で分を指定するとその間隔で発火する。0 は無効化ではなく 1 分に丸める。
    #[test]
    fn interval_gate_is_minutes_with_24h_default() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48)); // 位置（新規/過去の境界）は開けておく
        disable_backlog(&state, "a1"); // 新規側の間隔ゲートだけを見る
        seed_topic(&state, "a1", "n1", &hours_ago(3), 10);
        seed_topic(&state, "a1", "n2", &hours_ago(2), 11);
        assert_eq!(
            MemoryOrganizeConfig::default().min_interval_minutes,
            1440,
            "既定は 1440 分 = 24 時間（現行挙動）"
        );

        // 既定（1440 分）: throttle 刻時が 23h 前では通らない。
        let mut c = cfg(true, 10, 2);
        assert_eq!(c.min_interval_minutes, 1440);
        set_last_run(&state, "a1", &minutes_ago(23 * 60));
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Skip("interval_not_elapsed")
        ));

        // 10 分に詰めると、同じ刻時でも発火する。
        c.min_interval_minutes = 10;
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Run(_)
        ));
        // 5 分前 < 10 分 → まだ弾かれる（分の刻みが効いている）。
        set_last_run(&state, "a1", &minutes_ago(5));
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Skip("interval_not_elapsed")
        ));

        // 0 でもゲートは外れない（1 分に丸める）: 直後は弾かれ、2 分後は通る。
        c.min_interval_minutes = 0;
        set_last_run(
            &state,
            "a1",
            &(Utc::now() - Duration::seconds(10)).to_rfc3339(),
        );
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Skip("interval_not_elapsed")
        ));
        set_last_run(&state, "a1", &minutes_ago(2));
        assert!(matches!(
            decide_organize(&state.db, &c, "a1").unwrap(),
            OrganizeDecision::Run(_)
        ));
    }

    #[test]
    fn floor_gate_blocks_when_too_few_new_topics() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        // マーカーは 48h 前（間隔は通る）。新規 topic は 1 件だけ（下限 2 未満）。
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // 過去分が無い日を模す（新規側の下限だけを見る）。
        seed_topic(&state, "a1", "n0", &hours_ago(1), 10);
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        assert!(matches!(
            d,
            OrganizeDecision::Skip("below_floor_no_backlog")
        ));
    }

    #[test]
    fn snapshot_upper_bound_excludes_topics_beyond_watermark() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 100);
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // 新規側の snapshot 上端だけを見る（遡りは別テスト）。
                                       // snapshot 内 2 件 + snapshot 超過 2 件。超過分は下限にも worklist にも入らない。
        seed_topic(&state, "a1", "in1", &hours_ago(3), 50);
        seed_topic(&state, "a1", "in2", &hours_ago(2), 80);
        seed_topic(&state, "a1", "out1", &hours_ago(1), 200);
        seed_topic(&state, "a1", "out2", &hours_ago(1), 300);
        let d = decide_organize(&state.db, &cfg(true, 10, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 2, "snapshot 超過は数えない");
                let ids: Vec<&str> = plan.worklist.iter().map(|t| t.id.as_str()).collect();
                assert_eq!(ids, vec!["in1", "in2"]);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn gate_passes_builds_bounded_worklist_and_marker_boundary() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        disable_backlog(&state, "a1"); // このテストは新規側の bounded worklist と前進先を見る。
                                       // 既存タグを 1 つ用意（プロンプト同梱の語彙）。tag_topic はタグノードを新設する
                                       // （付与先 topic の実在はここでは問わない — 語彙の存在だけ用意する）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::tag_topic(
                &conn,
                "a1",
                "seedtopic",
                &["既存タグ".to_string()],
                "2026-08-01T00:00:00Z",
            )
            .unwrap();
        }
        // worklist 対象を 5 件（created_at 昇順）。
        for i in 1..=5 {
            let ts = (Utc::now() - Duration::hours(10 - i)).to_rfc3339();
            seed_topic(&state, "a1", &format!("n{i}"), &ts, 10 + i);
        }
        let d = decide_organize(&state.db, &cfg(true, 3, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 5, "下限判定は全 5 件");
                assert_eq!(plan.worklist_size, 3, "worklist は N=3 で bounded");
                assert_eq!(plan.new_presented, 3, "全て新規（過去分は無効化済み）");
                assert_eq!(plan.backlog_presented, 0, "過去分は 0 件");
                // 遡り側の枠は新規で埋まった（budget=3 を新規が使い切った）。
                assert!(plan.backlog_marker_advance_to.is_none(), "遡り側は据え置き");
                // 新規側の前進先は提示した末尾の (created_at, id) 複合カーソル（= n3）。
                let last = plan.worklist.last().unwrap();
                let advance = plan
                    .new_marker_advance_to
                    .as_deref()
                    .expect("新規を提示したので Some");
                assert_eq!(advance, format_cursor(&last.created_at, &last.id));
                // 再解釈すると (created_at, id) に戻る（parse ⇄ format の一貫性）。
                let (ts, id) = parse_cursor(advance);
                assert_eq!(ts, last.created_at);
                assert_eq!(id, last.id);
                // 既存タグの語彙がプロンプト材料に載る。
                assert!(plan.tags.iter().any(|(n, _)| n == "既存タグ"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // --- 過去分の遡り消化（#365 段階3b）---

    /// 新規が枠を使い切ったら過去分は 0 件（新規優先）。
    #[test]
    fn new_fills_budget_leaves_no_room_for_backlog() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48)); // 間隔は通る
        set_backlog_marker(&state, "a1", &hours_ago(240)); // 過去分の境界（10日前）
                                                           // 新規 3 件（境界 48h より後）。
        for i in 1..=3 {
            seed_topic(&state, "a1", &format!("n{i}"), &hours_ago(10 - i), 50 + i);
        }
        // 過去分も 3 件（境界 240h より古い）。
        for i in 1..=3 {
            seed_topic(
                &state,
                "a1",
                &format!("old{i}"),
                &hours_ago(300 - i),
                10 + i,
            );
        }
        // budget=2 を新規が使い切る。
        let d = decide_organize(&state.db, &cfg(true, 2, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_presented, 2, "新規が budget=2 を使い切る");
                assert_eq!(plan.backlog_presented, 0, "枠が無いので過去分は 0 件");
                assert_eq!(plan.worklist_size, 2);
                assert!(plan.backlog_marker_advance_to.is_none(), "遡り側は据え置き");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 枠が余ったら過去分で埋める（新規 → 過去分の順 / 合計 <= max_topics）。
    #[test]
    fn backlog_fills_leftover_after_new() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        // 新規 2 件（昇順 n1<n2）。
        seed_topic(&state, "a1", "n1", &hours_ago(5), 50);
        seed_topic(&state, "a1", "n2", &hours_ago(3), 60);
        // 過去分 4 件（270/280/290/300h 前）。
        for h in [270, 280, 290, 300] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        // budget=5: 新規 2 + 過去分 3（残り 1 は次回）。
        let d = decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_presented, 2);
                assert_eq!(plan.backlog_presented, 3, "残り枠 3 を過去分で埋める");
                assert_eq!(plan.worklist_size, 5, "合計は max_topics 以下");
                // 提示順は新規 → 過去分。先頭 2 件が新規。
                let ids: Vec<&str> = plan.worklist.iter().map(|t| t.id.as_str()).collect();
                assert_eq!(&ids[0..2], &["n1", "n2"], "新規が先");
                // 過去分は遡り（降順）: 270 → 280 → 290。
                assert_eq!(&ids[2..5], &["old270", "old280", "old290"]);
                // 遡り側マーカーは提示した中で最も古い old290 へ進む（old300 は次回）。
                let oldest = plan.worklist.last().unwrap();
                assert_eq!(oldest.id, "old290");
                assert_eq!(
                    plan.backlog_marker_advance_to.as_deref(),
                    Some(format_cursor(&oldest.created_at, &oldest.id).as_str())
                );
                assert_eq!(plan.backlog_remaining, 4, "遡り残数は提示前の 4 件");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 新規が無い日でも過去分が進む（下限は新規側だけを塞ぐ）。
    #[test]
    fn no_new_day_still_progresses_backlog() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48)); // 間隔は通る / 新規は 0 件
        set_backlog_marker(&state, "a1", &hours_ago(240));
        for h in [300, 290, 280] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        // 新規 0（下限 2 未満）だが過去分があるので発火する。
        let d = decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap();
        match d {
            OrganizeDecision::Run(plan) => {
                assert_eq!(plan.new_topic_count, 0, "新規は無い");
                assert_eq!(plan.new_presented, 0);
                assert_eq!(plan.backlog_presented, 3, "過去分だけで発火・進行する");
                // 新規側マーカーは**据え置き**（新規 0 件では壁時計 now へ飛ばさない / 恒久ロス防止）。
                assert!(
                    plan.new_marker_advance_to.is_none(),
                    "新規 0 件では新規側を進めない（None）"
                );
                // 遡り側は進む。日次 throttle は throttle 刻時（run_at）が担う。
                assert!(plan.backlog_marker_advance_to.is_some());
                assert!(
                    plan.run_at.parse::<DateTime<Utc>>().is_ok(),
                    "throttle 刻時は now"
                );
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// 遡りが先頭（最古）に到達したら止まる: 過去分 0 かつ新規 0 ならスキップ（無限に走らない）。
    #[test]
    fn backlog_head_reached_and_no_new_skips() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", EPOCH); // 遡りカーソルが先頭 → 過去分 0
                                                 // 過去 topic はあるが、全て境界（epoch）より新しい＝もう遡る先が無い。
        for h in [300, 290] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        let d = decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap();
        assert!(matches!(
            d,
            OrganizeDecision::Skip("below_floor_no_backlog")
        ));
    }

    /// 同じ topic を毎日拾い直さない（タグを付けなかったものも含めて / 位置マーカーで進む）。
    #[test]
    fn backlog_does_not_repick_presented_topics_across_runs() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        for h in [300, 290, 280] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        // run1: budget=2 → 遡り降順で old280, old290 を提示（タグ付けは一切しない）。
        let plan1 = match decide_organize(&state.db, &cfg(true, 2, 2), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids1: Vec<String> = plan1.worklist.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids1, vec!["old280", "old290"]);
        // clean 完了として前進させる（タグは付けていない）。run1 は過去分だけなので新規側は
        // 据え置き、遡り側と throttle 刻時が進む。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan1, true).unwrap();
        }
        // 翌日を模す: throttle 刻時を 48h 前へ戻して日次ゲートを開ける（遡りカーソルは
        // run1 の前進位置のまま = 別軸なので影響しない）。
        set_last_run(&state, "a1", &hours_ago(48));
        // run2: 次は old300 だけ（提示済みの old280/old290 は二度と出ない）。
        let plan2 = match decide_organize(&state.db, &cfg(true, 2, 2), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids2: Vec<String> = plan2.worklist.iter().map(|t| t.id.clone()).collect();
        assert_eq!(ids2, vec!["old300"], "提示済みを拾い直さない（未タグでも）");
    }

    /// partial（clean でない）では 2 軸マーカーも throttle 刻時も進めない。clean では全て進む。
    #[test]
    fn partial_run_does_not_advance_markers() {
        let state = crate::test_app_state();
        set_watermark(&state, "a1", 1000);
        set_marker(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        set_last_run(&state, "a1", &hours_ago(48));
        // 新規 2 件 + 過去分（両軸が進む計画にして、どちらも partial で止まることを見る）。
        seed_topic(&state, "a1", "n1", &hours_ago(5), 50);
        seed_topic(&state, "a1", "n2", &hours_ago(3), 60);
        for h in [300, 290] {
            seed_topic(&state, "a1", &format!("old{h}"), &hours_ago(h), 10 + h);
        }
        let plan = match decide_organize(&state.db, &cfg(true, 5, 2), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert!(plan.new_marker_advance_to.is_some(), "新規を提示（Some）");
        assert!(
            plan.backlog_marker_advance_to.is_some(),
            "過去分を提示（Some）"
        );
        let new_before = get_marker(&state, "a1");
        let backlog_before = get_backlog_marker(&state, "a1");
        let run_before = get_last_run(&state, "a1");
        // partial: clean=false → 何も進めない。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan, false).unwrap();
        }
        assert_eq!(
            get_marker(&state, "a1"),
            new_before,
            "partial で新規側は不変"
        );
        assert_eq!(
            get_backlog_marker(&state, "a1"),
            backlog_before,
            "partial で遡り側は不変"
        );
        assert_eq!(
            get_last_run(&state, "a1"),
            run_before,
            "partial で throttle 刻時も不変"
        );
        // clean=true → 2 軸 + throttle が計画どおり進む。
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan, true).unwrap();
        }
        assert_eq!(
            get_marker(&state, "a1"),
            plan.new_marker_advance_to,
            "clean で新規側が進む"
        );
        assert_eq!(
            get_backlog_marker(&state, "a1"),
            plan.backlog_marker_advance_to,
            "clean で遡り側が進む"
        );
        assert_eq!(
            get_last_run(&state, "a1").as_deref(),
            Some(plan.run_at.as_str()),
            "clean で throttle 刻時が now へ進む"
        );
    }

    /// 回帰（#365 レビュー / #364 と同型）: 非トランザクションなビルドが途中失敗して
    /// `end_log_id > watermark`（snapshot 外）の topic を残した状態で、過去分により整理ランが
    /// 発火し clean 完了しても、その topic を**新規側が恒久ロスしない**こと。
    ///
    /// 初版（新規 0 件で新規側を壁時計 `now` へ飛ばす）ではこのテストが落ちる:
    /// `new_marker_advance_to` が `Some(now)` になり（`is_none()` で失敗）、仮に進めれば run2 で
    /// stale が新規側カーソルに追い越されて worklist から消える。
    #[test]
    fn stale_topic_beyond_watermark_not_lost_on_zero_new_day() {
        let state = crate::test_app_state();
        // 位置・throttle を 48h 前に（間隔は通る）。遡り境界は 10 日前。
        set_marker(&state, "a1", &hours_ago(48));
        set_last_run(&state, "a1", &hours_ago(48));
        set_backlog_marker(&state, "a1", &hours_ago(240));
        // ビルドが step5(topic commit) 後 step7(watermark 更新) 前に失敗した状態を模す:
        // topic は commit 済みだが end_log_id=200 > watermark=100（snapshot 外）。created_at は現在より前。
        set_watermark(&state, "a1", 100);
        seed_topic(&state, "a1", "stale", &hours_ago(2), 200);
        // 過去分（遡り発火のトリガ）。end_log_id は watermark(100) 内にして snapshot に入れる
        // （stale だけが snapshot 外という状況を作る）。
        seed_topic(&state, "a1", "old300", &hours_ago(300), 10);
        seed_topic(&state, "a1", "old290", &hours_ago(290), 11);
        // run1: stale は snapshot 外で除外 → 新規 0。過去分で発火（下限 1 で run2 の 1 件でも発火）。
        let plan1 = match decide_organize(&state.db, &cfg(true, 5, 1), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(plan1.new_presented, 0, "stale は snapshot 外なので新規 0");
        assert!(
            plan1.new_marker_advance_to.is_none(),
            "新規 0 では新規側を壁時計へ飛ばさない（恒久ロス防止の肝）"
        );
        {
            let conn = state.db.lock().unwrap();
            advance_markers(&conn, "a1", &plan1, true).unwrap();
        }
        // ビルド再開で watermark が追いつく（stale が snapshot 内へ）。翌日を模す。
        set_watermark(&state, "a1", 300);
        set_last_run(&state, "a1", &hours_ago(48));
        // run2: stale が新規側で拾える（恒久ロスしない）。
        let plan2 = match decide_organize(&state.db, &cfg(true, 5, 1), "a1").unwrap() {
            OrganizeDecision::Run(p) => p,
            other => panic!("expected Run, got {other:?}"),
        };
        let ids: Vec<&str> = plan2.worklist.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ids.contains(&"stale"),
            "snapshot 外だった topic が新規側から恒久ロスした: {ids:?}"
        );
    }

    // --- プロンプト組み立て ---

