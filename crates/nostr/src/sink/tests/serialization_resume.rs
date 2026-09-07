    /// 同一セッションでは inbound 相当の respond と resume が直列化される。
    #[tokio::test]
    async fn resume_serializes_with_inbound_on_same_session() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(120));
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        // inbound 相当（watch ループと同じ入口）を走らせつつ、途中で完了 sink を発火。
        let r2 = r.clone();
        let sid2 = sid.clone();
        let inbound = tokio::spawn(async move {
            r2.respond_serialized(
                &sid2,
                "note1inbound",
                "suffix",
                Some("evt-1"),
                CallerIdentity::Agent,
                opencrab_actions::LiveInboundScope::AllOthers,
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        opencrab_actions::dispatch_settled(&r, settled(&sid, Some("note1resume")));

        inbound.await.unwrap();
        assert!(
            runner.wait_for_reply("note1resume").await,
            "resume も転記される"
        );
        // 直列化されているので LLM 実行が重なることはない。
        assert_eq!(
            runner.max_inflight.load(AtomicOrdering::SeqCst),
            1,
            "同一セッションの応答生成は同時に 1 本まで（二重回答の防止）"
        );
        assert_eq!(runner.runs.lock().unwrap().len(), 2);
    }

    /// 別セッション（#323 以降は**別エージェント**）は直列化されず並行する。
    #[tokio::test]
    async fn different_sessions_are_not_serialized() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok").with_delay(Duration::from_millis(150));
        let r = responder(runner.clone(), fake.cli());

        opencrab_actions::dispatch_settled(
            &r,
            settled(&nostr_session_id("agent-sink-a"), Some("note1a")),
        );
        opencrab_actions::dispatch_settled(
            &r,
            settled(&nostr_session_id("agent-sink-b"), Some("note1b")),
        );

        assert!(runner.wait_for_reply("note1a").await);
        assert!(runner.wait_for_reply("note1b").await);
        assert!(
            runner.max_inflight.load(AtomicOrdering::SeqCst) >= 2,
            "別セッションは並行して走れる"
        );
    }

    /// dispatch した subtask は session 共有 registry に載り、`cancel_subtask` から
    /// 到達できる（別 registry を渡すと常に not found になる回帰の防止 / #169）。
    #[tokio::test]
    async fn registry_is_shared_between_inbound_and_resume() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ok");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        let inbound_registry = r.runtime().registry_for(&sid);

        // **応答生成に実際に渡された登録簿**が、停止処理が引くものと同一 Arc であること。
        //
        // ここを `registry_for(&sid)` 同士の比較で書くと `SubtaskRegistries` の恒真式に
        // なり、`respond` 側が別インスタンスを渡す壊れ方を 1 件も検知できない（実際、
        // 旧テストは `sink.rs` の `registry_for(session_id)` を新規 DashMap に差し替えても
        // 緑のままだった / #203 の一括点検）。捕まえたいのは配線なので、`FakeRunner` が
        // 捕捉した `RunRequest` の中身を見る（`web-gateway` の
        // `run_uses_the_gateways_registry_so_cancel_can_reach_it` と同じ形）。
        //
        // inbound（watch ループの入口）と resume（完了 sink）の**両経路**を見る:
        // どちらか一方だけ配線が外れても停止が届かなくなる。
        r.respond_serialized(
            &sid,
            "note1inbound",
            "suffix",
            Some("evt-1"),
            CallerIdentity::Agent,
            opencrab_actions::LiveInboundScope::AllOthers,
        )
        .await;
        opencrab_actions::dispatch_settled(&r, settled(&sid, Some("note1resume")));
        assert!(
            runner.wait_for_reply("note1resume").await,
            "resume が走ること"
        );

        {
            let runs = runner.runs.lock().unwrap();
            assert_eq!(runs.len(), 2, "inbound と resume で 2 回走る");
            for (label, obs) in [("inbound", &runs[0]), ("resume", &runs[1])] {
                let observed = obs
                    .3
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}: run に登録簿が載っていない"));
                assert!(
                    Arc::ptr_eq(observed, &inbound_registry),
                    "{label}: 応答生成に渡した登録簿が、停止処理が引くものと別インスタンス\
                     になっている（cancel_subtask が常に not found になる）"
                );
            }
        }

        // 走行中 subtask を模して登録 → has_running が真。
        inbound_registry.insert(
            "st-live".to_string(),
            opencrab_actions::SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: "subtask-st-live".to_string(),
                parent_session_id: sid.clone(),
                agent_id: "agent-sink-test".to_string(),
                label: "nostr_generate_key(sunny)".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: Some("note1target".to_string()),
                caller: opencrab_actions::CallerIdentity::Agent,
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
                steerable: false,
            },
        );
        assert!(r.runtime().has_running(&sid));

        // 同じ registry を引く `cancel_subtask`（server-neutral / #161）で停止できる。
        let db = opencrab_db::Db::memory().unwrap();
        let outcome = opencrab_actions::cancel_subtask(
            &r.runtime().registry_for(&sid),
            &db,
            None,
            None,
            "st-live",
            opencrab_actions::CallerIdentity::Agent,
            Some(&sid),
        );
        assert_eq!(outcome, opencrab_actions::CancelOutcome::Cancelled);
        assert!(!r.runtime().has_running(&sid));
    }

    /// **#445**（#443 の同型）: 完了以外の決着で「完了しました」と断言しない。
    ///
    /// resume を起こす `SettleKind::Completed` は timeout / error / stopped_by_limit でも
    /// 発火するので、一律「完了」と告げると同じ prompt のマーカー（`exit_reason=timeout`）
    /// と矛盾する。各決着の述部が入り、マーカーは生の `exit_reason` をそのまま持つことも見る。
    #[test]
    fn resume_suffix_never_claims_completion_for_unfinished_subtasks() {
        for (exit_reason, expected) in [
            ("timeout", "時間切れで打ち切られました"),
            ("error", "エラーで失敗しました"),
            ("stopped_by_limit", "反復上限に達して途中で打ち切られました"),
            // 未知の値は断定しない（`subtask.rs` が語彙を増やしても誤情報にならない）。
            ("weird_new_reason", "終了しました"),
        ] {
            let suffix = resume_prompt_suffix("note1target", "st-1", exit_reason);
            assert!(
                !suffix.contains("完了しました"),
                "exit_reason={exit_reason} で完了を断言している: {suffix}"
            );
            assert!(
                suffix.contains(expected),
                "exit_reason={exit_reason} の決着が伝わらない: {suffix}"
            );
            assert!(
                suffix.contains(&format!("exit_reason={exit_reason}]")),
                "マーカーは生の exit_reason をそのまま持つ: {suffix}"
            );
        }
    }

    /// 完了した subtask だけが「完了しました」を受け取る（従来の文言）。
    #[test]
    fn resume_suffix_states_completion_only_when_completed() {
        let suffix = resume_prompt_suffix("note1target", "st-1", "completed");
        assert!(
            suffix.contains("バックグラウンド処理が完了しました"),
            "完了は完了と伝える: {suffix}"
        );
    }

    /// #588: 返信先ノートが無い（ブロードキャストの時刻発火）resume は、`nostr_reply` の空 target
    /// ではなく `nostr_post` の新規投稿へ誘導する。
    #[test]
    fn resume_suffix_guides_to_post_when_no_reply_target() {
        let suffix = resume_prompt_suffix("", "st-1", "completed");
        assert!(
            suffix.contains("nostr_post で投稿"),
            "返信先が無ければ新規投稿へ誘導: {suffix}"
        );
        assert!(
            !suffix.contains("nostr_reply(target=\"\")"),
            "空の返信先へ返信させない: {suffix}"
        );
    }
