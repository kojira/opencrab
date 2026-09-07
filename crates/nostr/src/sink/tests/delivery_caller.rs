    /// resume は応答を `reply_target` 宛アンカー付きでセッションへ転記する（session_id からは
    /// 復元できない宛先を記録に残す）。#588: 配送は機構が行わない（エージェントがツールで送る）ので、
    /// ここは**記録**を見る。
    #[tokio::test]
    async fn sink_records_reply_with_target_anchor() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("鍵ができました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        opencrab_actions::dispatch_settled(&r, settled(&sid, Some("note1target")));

        assert!(
            runner.wait_for_reply("note1target").await,
            "reply_target 宛アンカー付きで転記されるべき: replies={:?}",
            runner.replies.lock().unwrap()
        );
        // 機構は publish しない（配送はエージェントのツール）。
        assert!(
            fake.sent().is_empty(),
            "機構は暗黙返信しない: {}",
            fake.sent()
        );
        // 記録には本文 + 宛先アンカーが載る。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].2.contains("鍵ができました")
                && replies[0].2.contains("[Nostr reply target=note1target]"),
            "記録は本文 + 宛先アンカー: {}",
            replies[0].2
        );
        // resume も dispatch 有効（registry + sink）で走り、reply_target を引き継ぐ。
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, sid);
        assert_eq!(runs[0].1.as_deref(), Some("note1target"));
        assert!(runs[0].2, "resume も非ブロック dispatch を有効化する");
        // #323 / B2: resume は相手が不定なので走行中注入は Silent（別相手の誤爆防止）。
        assert_eq!(runs[0].5, "silent", "resume の走行中注入は Silent");
    }

    /// [#323 / B1] outbound の記録には返信先アンカーが載る（記録専用 / inbound_anchor と対称）。
    /// tool_call 行を作らない転記経路なので、これが無いと「この返信が誰宛か」を復元できない。
    /// #588: 機構は publish しないので `fake.sent()` は空（配送はエージェントのツール）。
    #[tokio::test]
    async fn outbound_record_carries_reply_target_anchor() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("返答本文");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.respond_serialized(
            &sid,
            "note1target",
            "suffix",
            Some("evt-1"),
            CallerIdentity::Agent,
            opencrab_actions::LiveInboundScope::OnlySpeaker("pk-peer".to_string()),
        )
        .await;

        // 記録された outbound には宛先アンカーが載る（誰宛か復元可能）。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].2.contains("返答本文")
                && replies[0].2.contains("[Nostr reply target=note1target]"),
            "記録は本文 + 宛先アンカー: {}",
            replies[0].2
        );
        // #588: 機構は暗黙返信しない（配送はエージェントが nostr_reply 等で行う）。
        assert!(
            fake.sent().is_empty(),
            "機構は publish しない: {}",
            fake.sent()
        );
    }

    // ---- #319: 呼び出し元は導出せず、呼び出し側から受け取る ----

    /// **本丸（inbound）**: 渡された呼び出し元がそのまま run に載る。
    ///
    /// 以前はここが `CallerIdentity::Agent` 固定で、オーナー発のターンでも
    /// OWNER_ONLY / TRUSTED_ONLY のツールが list にも dispatch にも出なかった（#319）。
    /// 発言者の解決は受信イベントの `pubkey` を持つ `handle_event` の責務で、
    /// ここでは**受け取った値をそのまま使う**（session_id からの逆算はしない）。
    #[tokio::test]
    async fn inbound_turn_uses_the_caller_it_was_given() {
        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::TrustedUser,
            CallerIdentity::Agent,
        ] {
            let fake = FakeNostaro::new();
            let runner = FakeRunner::new("応答");
            let r = responder(runner.clone(), fake.cli());
            let sid = nostr_session_id("agent-sink-test");

            r.respond_serialized(
                &sid,
                "note1target",
                "suffix",
                Some("evt-1"),
                caller.clone(),
                opencrab_actions::LiveInboundScope::AllOthers,
            )
            .await;

            let runs = runner.runs.lock().unwrap();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].4, caller, "渡した呼び出し元が run に載っていない");
        }
    }

    /// **本丸（resume）**: subtask 完了 resume は親 run の呼び出し元
    /// （`SubtaskSettled.caller` / #298）を引き継ぐ。
    ///
    /// ここが `Agent` 固定だったため、オーナー発のターンでも subtask が決着した瞬間に
    /// 権限が降格していた（`report_progress` を呼ぶと自分の権限が落ちる、という自爆）。
    #[tokio::test]
    async fn resume_turn_inherits_the_parent_caller() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("完了しました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        opencrab_actions::dispatch_settled(
            &r,
            settled_with_caller(&sid, Some("note1target"), CallerIdentity::Owner),
        );
        assert!(
            runner.wait_for_reply("note1target").await,
            "resume が走ること"
        );

        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].4,
            CallerIdentity::Owner,
            "resume で親ターンの権限が落ちている"
        );
    }

    /// 引き継ぐだけで**昇格はしない**: 親が最小権限なら resume も最小権限のまま。
    #[tokio::test]
    async fn resume_does_not_escalate_a_least_privileged_parent() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("完了しました");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        opencrab_actions::dispatch_settled(
            &r,
            settled_with_caller(&sid, Some("note1target"), CallerIdentity::Agent),
        );
        assert!(runner.wait_for_reply("note1target").await);

        assert_eq!(
            runner.runs.lock().unwrap()[0].4,
            CallerIdentity::Agent,
            "resume で権限が上がった"
        );
    }

    /// #588 / #440: `reply_target` が無くても（ブロードキャストの時刻発火など）継続は起こる
    /// （判定は session_id の一致だけ）。応答はセッションへアンカー無しで転記され、publish はしない
    /// （配送はエージェントのツール）。以前は「返信先が無ければ resume しない」だったが、その根拠
    /// （届かない応答を転記してしまう）は暗黙返信の撤去で消えた。
    #[tokio::test]
    async fn resume_without_reply_target_records_but_does_not_publish() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ブロードキャストの続き");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        opencrab_actions::dispatch_settled(&r, settled(&sid, None));
        // 空白のみも「返信先なし」扱い（正規化される）。
        opencrab_actions::dispatch_settled(&r, settled(&sid, Some("   ")));

        assert!(
            runner.wait_for_reply("ブロードキャストの続き").await,
            "返信先が無くても継続ターンが走ってセッションへ転記される: replies={:?}",
            runner.replies.lock().unwrap()
        );
        // 機構は publish しない（配送はエージェントのツール）。
        assert!(fake.sent().is_empty(), "機構は送信しない: {}", fake.sent());
        // 転記は本文のみ（返信先が無いのでアンカーは付かない）。
        let replies = runner.replies.lock().unwrap();
        assert!(
            replies
                .iter()
                .all(|r| !r.2.contains("[Nostr reply target=")),
            "返信先なしの転記にアンカーは付かない: {replies:?}"
        );
    }

    /// 非 Nostr セッションの settle は無視する（web / heartbeat のネスト等）。
    #[tokio::test]
    async fn sink_ignores_non_nostr_sessions() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("x");
        let r = responder(runner.clone(), fake.cli());

        opencrab_actions::dispatch_settled(&r, settled("web-agent-x-conv1", Some("note1target")));
        opencrab_actions::dispatch_settled(&r, settled("heartbeat-agent-x", Some("note1target")));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(fake.sent().is_empty());
        assert!(runner.runs.lock().unwrap().is_empty());
    }

    /// NO_REPLY / 空応答なら送信しない（沈黙の尊重）。
    #[tokio::test]
    async fn no_reply_response_is_not_delivered() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("NO_REPLY");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        let out = r
            .respond_serialized(
                &sid,
                "note1target",
                "suffix",
                None,
                CallerIdentity::Agent,
                opencrab_actions::LiveInboundScope::AllOthers,
            )
            .await;
        assert!(out.is_none());
        assert!(fake.sent().is_empty());
        // 転記もしない（送っていない応答を履歴に残さない）。
        assert!(runner.replies.lock().unwrap().is_empty());
    }

    /// #588: 配送はエージェントの明示送信だけ。モデルが `nostr_reply` を実行したものが届き、
    /// 機構はそれに**加えて**送ったりしない（暗黙返信は撤去済み）。送信は 1 回だけ。
    #[tokio::test]
    async fn only_the_agents_explicit_send_is_delivered() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("本文").with_explicit_reply("note1explicit");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        let out = r
            .respond_serialized(
                &sid,
                "note1implicit",
                "suffix",
                Some("evt-1"),
                CallerIdentity::Agent,
                opencrab_actions::LiveInboundScope::OnlySpeaker("pk-peer".to_string()),
            )
            .await;
        assert_eq!(out.as_deref(), Some("本文"));

        // DI フェーズ1: legacy sink の組み込み publish ツール（nostr_post/reply）は撤去済み。
        // 名前指定の nostr_reply は fail-closed で publish されず、機構も reply_target（implicit）へ
        // 送らない → 何も publish されない（返信は V3/DI reply 操作が担う）。応答本文の転記は残る。
        let sent = fake.sent();
        assert!(
            !sent.contains("note1explicit"),
            "撤去済み nostr_reply は publish しない: {sent}"
        );
        assert!(
            !sent.contains("note1implicit"),
            "機構は reply_target へ暗黙返信しない: {sent}"
        );
        // 応答本文の転記は行う（会話履歴の継続性）。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        // #323 / B1: 記録には宛先アンカーが焼かれる（「誰宛か」を復元できる）。
        // アンカーの target はこのターンの reply_target。
        assert!(
            replies[0].2.contains("[Nostr reply target=note1implicit]"),
            "記録に宛先アンカーが載る: {}",
            replies[0].2
        );
        // #323 / B2: respond の scope が RunRequest まで配線されている。
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs[0].5, "only:pk-peer", "走行中注入の対象範囲を配線する");
    }

    /// #588: 明示送信が無ければ**何も publish されない**が、応答はセッションへ転記される
    /// （オーナー指示: 返信先があってもツールを呼ばなければ出ない。履歴には残る）。
    #[tokio::test]
    async fn no_explicit_send_records_but_does_not_publish() {
        let fake = FakeNostaro::new();
        let runner = FakeRunner::new("ツールを呼ばない応答");
        let r = responder(runner.clone(), fake.cli());
        let sid = nostr_session_id("agent-sink-test");

        r.respond_serialized(
            &sid,
            "note1implicit",
            "suffix",
            Some("evt-1"),
            CallerIdentity::Agent,
            opencrab_actions::LiveInboundScope::AllOthers,
        )
        .await;
        // 機構は publish しない。
        assert!(
            fake.sent().is_empty(),
            "ツール未使用なら何も出ない: {}",
            fake.sent()
        );
        // ただしセッションへは転記される（本文 + 宛先アンカー）。
        let replies = runner.replies.lock().unwrap();
        assert_eq!(replies.len(), 1);
        assert!(
            replies[0].2.contains("ツールを呼ばない応答")
                && replies[0].2.contains("[Nostr reply target=note1implicit]"),
            "本文 + 宛先アンカーを記録: {}",
            replies[0].2
        );
    }

