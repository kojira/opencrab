    /// [P1 回帰 / #168] 同一セッション（同一相手）の連投は**投入順どおり**に処理される。
    ///
    /// 応答生成を素朴に `tokio::spawn` へ出していたときは「どの spawn タスクが先に
    /// session ロックを取るか」で順序が決まり、5 通目への返信が 1 通目より先に飛ぶ
    /// ことがあった（各返信は勝ったタスクの `reply_target` に紐づく）。
    /// multi_thread ランタイムで複数回試行して安定することを見る。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_session_events_are_processed_in_submission_order() {
        const IDS: [&str; 8] = [
            "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth",
        ];
        let expected: Vec<String> = IDS.iter().map(|i| format!("note1{i}")).collect();

        // spawn 順の運任せを暴くには複数回試行が必要（1 回だと偶然通ることがある）。
        for trial in 0..5 {
            let h = Harness::new("agent-order", Duration::from_millis(5), 8, 32);
            for id in IDS {
                h.feed(id, "pk-chatty", id).await;
            }
            assert!(
                h.wait_finished(IDS.len(), Duration::from_secs(5)).await,
                "試行{trial}: 応答生成が完了しない"
            );

            // 転記順・開始順・完了順（= 返信が飛ぶ順）がすべて投入順と一致する。
            // 転記本文は「本文 + 受信メタアンカー」（#282）なので本文の先頭一致で見る。
            assert_eq!(
                SlowRunner::snapshot(&h.runner.recorded)
                    .iter()
                    .map(|t| t.lines().next().unwrap_or_default().to_string())
                    .collect::<Vec<_>>(),
                IDS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "試行{trial}: 会話への転記順が投入順と違う"
            );
            assert_eq!(
                SlowRunner::snapshot(&h.runner.started),
                expected,
                "試行{trial}: 同一セッションの処理順が投入順と違う"
            );
            assert_eq!(
                SlowRunner::snapshot(&h.runner.finished),
                expected,
                "試行{trial}: 返信順が投入順と違う"
            );
            // 同一セッションは直列（二重投稿しない）。
            assert_eq!(
                h.runner.max_inflight.load(AtomicOrdering::SeqCst),
                1,
                "試行{trial}: 同一セッションの応答生成が並行した"
            );
        }
    }

    /// [#323] 相手が違っても受信は**同じ session**に落ちる（agent 単位で 1 会話）。
    ///
    /// 旧規約 `nostr-{agent}-{author_pubkey}` は会話を相手ごとに割っていたため、
    /// エージェントは「自分がさっき誰に何を言ったか」を跨いで見られず、同じ内容を
    /// 繰り返したり自分の発言と食い違うことを言った（#323）。Nostr のスレッドは
    /// そもそも多人数なので、「1 相手 = 1 会話」という前提自体が合っていない。
    ///
    /// **発言者の区別は session ではなく `speaker_id` が担う**。転記の `sender_id`
    /// （= 相手の pubkey）はイベントごとに入るので、1 本に混ざっても誰の発言かは
    /// 失われない（会話文字列は `[{speaker_id}]:` で出る）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn events_from_different_authors_share_one_session() {
        let h = Harness::new("agent-one-session", Duration::from_millis(5), 8, 32);

        h.feed("m1", "pk-alice", "1件目").await;
        h.feed("m2", "pk-bob", "2件目").await;
        assert!(
            h.wait_finished(2, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );

        let expected = vec!["nostr-agent-one-session".to_string(); 2];
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_sessions),
            expected,
            "相手が違っても転記先の session は 1 本"
        );
        assert_eq!(
            SlowRunner::snapshot(&h.runner.run_sessions),
            expected,
            "応答生成も同じ session で走る（履歴が揃う）"
        );
        // 1 本に混ざっても「誰の発言か」は転記の speaker_id で区別が付く。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_speakers),
            vec!["pk-alice".to_string(), "pk-bob".to_string()],
            "発言者が session に潰されている（プロンプトで相手を区別できない）"
        );
    }

    /// [#323] 同一エージェントの応答生成は、**相手が違っても**直列化される。
    ///
    /// 「発言し終わるまで次の LLM を呼ばない」（オーナー方針）。直列化の鍵は
    /// `SessionRuntime` の session_id なので、session が agent 単位で 1 本になれば
    /// 追加の仕掛け無しにそのまま成り立つ。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn responses_of_one_agent_are_serialized_across_authors() {
        let h = Harness::new("agent-serial", Duration::from_millis(80), 8, 32);

        h.feed("s1", "pk-alice", "1").await;
        h.feed("s2", "pk-bob", "2").await;
        assert!(
            h.wait_finished(2, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );

        assert_eq!(
            h.runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "同一エージェントの応答生成が並行した（相手ごとに割れている）"
        );
        assert_eq!(
            SlowRunner::snapshot(&h.runner.finished),
            vec!["note1s1".to_string(), "note1s2".to_string()],
            "投入順どおりに 1 件ずつ返る"
        );
    }

    /// [P1 回帰 / #178] 受信ループは応答生成を await しない。
    ///
    /// 以前は `respond_serialized(...).await` をループ内で直接呼んでいたため、長い応答の
    /// あいだ**全セッション・全相手**の受信が止まった（`nostaro watch` の stdout も
    /// 読まれず滞留）。ここでは 2 件の `handle_event` が即座に返ること、かつ別セッション
    /// の応答が並行することを見る。
    ///
    /// #323 で session は agent 単位になったので、「別セッション」= 別エージェント。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handle_event_does_not_block_the_receive_loop() {
        let h = Harness::new("agent-loop", Duration::from_millis(300), 8, 32);

        let started = std::time::Instant::now();
        // 別セッション（別エージェント）から 2 件。ループ相当の直列呼び出し。
        h.feed("e1", "pk-a", "1件目").await;
        h.feed_as("agent-loop-2", "e2", "pk-b", "2件目").await;
        let elapsed = started.elapsed();

        // ループは応答生成（300ms）を待たずに次へ進んでいる。
        assert!(
            elapsed < Duration::from_millis(150),
            "受信ループが応答生成でブロックしている: {elapsed:?}"
        );
        // 受信の転記はループ内で同期的に済んでいる（順序も保たれる）。
        // 本文の後ろに受信メタアンカーが付く（#282）ので先頭行で比べる。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded)
                .iter()
                .map(|t| t.lines().next().unwrap_or_default().to_string())
                .collect::<Vec<_>>(),
            vec!["1件目".to_string(), "2件目".to_string()]
        );

        // 別セッションの応答生成は並行して走る。
        for _ in 0..100 {
            if h.runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            h.runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
            "別セッションの応答生成が並行していない（head-of-line blocking）"
        );
    }

    /// [P1 回帰 / #178] permit 待ちが**受信を止めない**（head-of-line blocking なし）。
    ///
    /// permit を受信ループ内で取っていたときは、ロック待ちで何もしていないタスクが permit
    /// を占有し、上限が埋まった時点でループ全体（＝全受信）が停止した。レビュアーの
    /// 実験と同型: permits=2 / 同一セッション 2 件 → 別セッション 1 件。別セッションの
    /// 応答生成が、詰まっているセッションの完了を待たずに始まることを見る。
    ///
    /// #323 で session は agent 単位になったので、「別セッション」= 別エージェント。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permit_starvation_does_not_stall_the_receive_loop() {
        for trial in 0..3 {
            let h = Harness::new("agent-starve", Duration::from_millis(300), 2, 32);

            // 多弁なセッションが permit を使い切ろうとする。
            h.feed("s1", "pk-chatty", "1").await;
            h.feed("s2", "pk-chatty", "2").await;
            // 別セッション。ここでループが止まってはいけない。
            let started = std::time::Instant::now();
            h.feed_as("agent-starve-2", "s3", "pk-quiet", "3").await;
            let loop_stall = started.elapsed();

            assert!(
                loop_stall < Duration::from_millis(50),
                "試行{trial}: 別セッションの handle_event がループを止めた: {loop_stall:?}"
            );

            // 別セッションの応答生成は、詰まっているセッション（300ms×2）を待たずに始まる。
            let mut quiet_started = false;
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            while std::time::Instant::now() < deadline {
                if SlowRunner::snapshot(&h.runner.started)
                    .iter()
                    .any(|t| t == "note1s3")
                {
                    quiet_started = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                quiet_started,
                "試行{trial}: 別セッションの応答生成が同一セッションの完了を待たされた"
            );
            // 同時に 2 本走った = permit がロック待ちに浪費されていない。
            assert!(
                h.runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
                "試行{trial}: permit がロック待ちのタスクに占有されている"
            );
        }
    }

    /// [#178] permit をタスク内側で取っても同時実行上限は守られる。
    ///
    /// permit=1 なら別セッション 3 件でも応答生成は 1 本ずつ（`max_inflight == 1`）。
    /// ループ自体はブロックしない（上限は実同時実行だけを絞る）。
    ///
    /// #323 以降、別セッション = 別エージェント。同一セッションで流すと per-session
    /// 直列化だけで `max_inflight == 1` になり、**permit の有無を検知できない**
    /// （上限を消しても緑のままになる）ので、必ずセッションを分けて流す。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_responses_are_capped_by_permits() {
        let h = Harness::new("agent-cap", Duration::from_millis(80), 1, 32);

        let started = std::time::Instant::now();
        h.feed_as("agent-cap-1", "c1", "pk-a", "1").await;
        h.feed_as("agent-cap-2", "c2", "pk-b", "2").await;
        h.feed_as("agent-cap-3", "c3", "pk-c", "3").await;
        let loop_stall = started.elapsed();
        assert!(
            loop_stall < Duration::from_millis(50),
            "上限が受信ループを止めている: {loop_stall:?}"
        );

        assert!(
            h.wait_finished(3, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );
        assert_eq!(
            h.runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "permit=1 のとき応答生成は 1 本ずつ"
        );
        // 直列化されたぶんの時間はかかっている（上限が実在する証拠）。
        assert!(
            started.elapsed() >= Duration::from_millis(240),
            "同時実行上限（permit）が効いていない: {:?}",
            started.elapsed()
        );
    }

    /// [#168] session キューが溢れたぶんは**ログに残して**捨てる（黙って捨てない）。
    ///
    /// permit を 0 本にして consumer を確実に止め、capacity を超える連投を流し込む。
    /// 受け付けられるのは「consumer が取り出した 1 件 + バッファ capacity 件」まで。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_queue_overflow_is_dropped_and_counted() {
        const CAPACITY: usize = 2;
        const FLOOD: usize = 20;
        // permits=0 → consumer は permit 待ちで止まり、キューが確実に埋まる。
        let h = Harness::new("agent-flood", Duration::from_millis(1), 0, CAPACITY);

        for i in 0..FLOOD {
            h.feed(&format!("f{i}"), "pk-flood", &format!("{i}")).await;
        }

        // 受理されうる上限は inflight 1 + バッファ CAPACITY。
        let accepted = FLOOD as u64 - h.queues.dropped();
        assert!(
            accepted <= (1 + CAPACITY) as u64,
            "キュー上限を超えて受理された: accepted={accepted}"
        );
        assert!(
            h.queues.dropped() >= (FLOOD - 1 - CAPACITY) as u64,
            "溢れが捨てられていない: dropped={}",
            h.queues.dropped()
        );
        // 捨てても投稿本文は会話履歴に転記済み（次の応答の文脈に載る）。
        assert_eq!(SlowRunner::snapshot(&h.runner.recorded).len(), FLOOD);
    }

    /// [#168] アイドルになった session の consumer タスク / チャネルは回収される。
    ///
    /// #323 でセッションが 1 本になっても回収が壊れないことを、複数セッション
    /// （= 複数エージェント）を並べたまま確かめる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_session_queue_is_reclaimed() {
        let h = Harness::new("agent-reclaim", Duration::from_millis(5), 8, 32);
        h.feed("r1", "pk-a", "1").await;
        h.feed_as("agent-reclaim-2", "r2", "pk-b", "2").await;
        assert_eq!(
            h.queues.active_sessions(),
            2,
            "投入直後は session ごとにキューが存在する"
        );

        assert!(
            h.wait_finished(2, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );
        // 完了後、キューは空になり consumer は自分ごと回収される。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && h.queues.active_sessions() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            h.queues.active_sessions(),
            0,
            "アイドルな session キューが回収されていない（task/チャネルのリーク）"
        );

        // 回収後に再投入しても普通に処理される（回収とレースしても取りこぼさない）。
        h.feed("r3", "pk-a", "3").await;
        assert!(
            h.wait_finished(3, Duration::from_secs(5)).await,
            "回収後の再投入が処理されない"
        );
    }

