    /// [#252 段階 A] 転記が有効なら、受信 1 件につき配送口が**ちょうど 1 回**呼ばれる。
    /// 転記本文には送信者ラベル・種別・本文が載る。転記は受信ループ内で同期的に済む。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_fires_once_per_inbound_when_configured() {
        const URL: &str = "https://discord.com/api/webhooks/1/tok";
        let runner = SlowRunner::new(Duration::from_millis(1)).with_relay_target(URL);
        let h = Harness::with_runner("agent-relay", runner, 8, 32);

        h.feed("r1", "pk-a", "こんにちは").await;

        let relayed = SlowRunner::snapshot(&h.runner.relayed);
        assert_eq!(relayed.len(), 1, "受信 1 件につき転記は 1 回");
        assert!(
            relayed[0].contains("こんにちは"),
            "本文が載る: {}",
            relayed[0]
        );
        // author_label は name/npub 無しなので短縮 pubkey。種別見出しも載る。
        assert!(relayed[0].contains("pk-a"), "送信者が載る: {}", relayed[0]);
        assert!(
            relayed[0].contains("メンション") || relayed[0].contains("[Nostr"),
            "種別見出しが載る: {}",
            relayed[0]
        );

        // 2 件目も 1 回ずつ増える（受信ごとに 1 回）。
        h.feed("r2", "pk-a", "ふたつめ").await;
        assert_eq!(SlowRunner::snapshot(&h.runner.relayed).len(), 2);
    }

    /// [#252 段階 A / fail-closed] 転記が未設定なら、受信があっても 1 件も飛ばない。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_is_fail_closed_when_unconfigured() {
        // relay_target を設定しない runner（= resolve が None を返す）。
        let h = Harness::new("agent-norelay", Duration::from_millis(1), 8, 32);

        h.feed("n1", "pk-a", "本文").await;
        h.feed("n2", "pk-b", "本文2").await;

        assert!(
            SlowRunner::snapshot(&h.runner.relayed).is_empty(),
            "未設定なら転記は 1 件も飛ばない（fail-closed）"
        );
        // 受信自体は通常どおり転記（会話履歴）される。
        assert_eq!(SlowRunner::snapshot(&h.runner.recorded).len(), 2);
    }

    /// [#514] DM（kind:4 / 1059）は会話へ入らない: 記録も応答生成も転記も起きない。
    ///
    /// テスト 1（会話へ入らない）＋ テスト 4 の対（通常 kind は従来どおり）。
    /// **変異確認**: `handle_event` 冒頭の `if event.is_dm() { return; }` を外すと、
    /// DM が record/started に現れてこのテストが赤くなる。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dm_is_dropped_before_entering_conversation() {
        const URL: &str = "https://discord.com/api/webhooks/1/tok";
        // 転記も有効にして「DM は転記もされない」まで見る。
        let runner = SlowRunner::new(Duration::from_millis(1)).with_relay_target(URL);
        let h = Harness::with_runner("agent-dm-drop", runner, 8, 32);

        for &kind in crate::event::DM_KINDS {
            let mut ev = rich_event(kind);
            ev.id = format!("dm{kind}");
            h.feed_event(ev).await;
        }

        // 会話履歴に入らない（記録ゼロ）。
        assert!(
            SlowRunner::snapshot(&h.runner.recorded).is_empty(),
            "DM は会話履歴へ記録されない"
        );
        // 転記（Discord webhook）へも回らない。
        assert!(
            SlowRunner::snapshot(&h.runner.relayed).is_empty(),
            "DM は Discord へ転記されない"
        );
        // 応答生成が起きない＝返信 publish 経路に一切入らない（テスト 2: kind:1 の
        // 公開リプライで返した事故の回帰）。少し待っても started は空のまま。
        assert!(
            !h.wait_finished(1, Duration::from_millis(200)).await,
            "DM で応答生成が走ってはいけない"
        );
        assert!(
            SlowRunner::snapshot(&h.runner.started).is_empty(),
            "DM で run_agent_response（＝返信 publish 経路）が呼ばれない"
        );

        // 対照: 通常の kind:1 は従来どおり記録され、応答生成が走る（テスト 4）。
        h.feed_event(rich_event(1)).await;
        assert!(
            h.wait_finished(1, Duration::from_secs(2)).await,
            "通常ノートは従来どおり処理される"
        );
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded).len(),
            1,
            "通常ノートは 1 件記録される（DM は数に入らない）"
        );
    }

    /// メタ情報の検証用イベント（npub / note_id / kind を明示的に持つ / #282）。
    fn rich_event(kind: u32) -> NostrEvent {
        NostrEvent {
            id: "deadbeefid".to_string(),
            pubkey: "0011223344556677".to_string(),
            npub: Some("npub1author".to_string()),
            note_id: Some("note1target".to_string()),
            author_name: Some("owner".to_string()),
            created_at: 1_700_000_000,
            kind,
            content: "こんにちは".to_string(),
            tags: Vec::new(),
        }
    }

    /// [#282] 会話履歴に残る本文へ、author の npub / note id / kind が焼き込まれる。
    /// 本文だけを記録していた劣化（nostaro 本体より情報が少ない）の回帰防止。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_record_carries_author_note_and_kind() {
        let h = Harness::new("agent-meta", Duration::from_millis(1), 8, 32);
        h.feed_event(rich_event(1)).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        let text = &recorded[0];
        assert!(text.contains("こんにちは"), "本文が残る: {text}");
        assert!(
            text.contains("npub1author"),
            "author の npub が残る: {text}"
        );
        assert!(text.contains("note1target"), "note id が残る: {text}");
        assert!(text.contains("kind:1"), "kind が残る: {text}");
        assert!(text.contains("メンション"), "種別ラベルが残る: {text}");
    }

    /// [#282] npub / note_id が無い（None）受信でも、アンカーは壊れず hex へフォールバック
    /// する（空の `from=` / `target=` が並ばない）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_record_anchor_survives_missing_optional_fields() {
        let h = Harness::new("agent-meta-none", Duration::from_millis(1), 8, 32);
        let mut ev = rich_event(1);
        ev.npub = None;
        ev.note_id = None;
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        let text = &recorded[0];
        assert!(
            !text.contains("from=]") && !text.contains("from= "),
            "空の from= が残らない: {text}"
        );
        assert!(
            text.contains("from=0011223344556677"),
            "pubkey へフォールバックする: {text}"
        );
        assert!(
            text.contains("target=deadbeefid"),
            "hex id へフォールバックする: {text}"
        );
    }

    /// [#282] `prompt_suffix` にも npub / pubkey / note id / kind が事実として載る
    /// （「target=… を使え」という指示だけだった劣化の回帰防止）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_suffix_carries_author_note_and_kind() {
        let h = Harness::new("agent-suffix", Duration::from_millis(1), 8, 32);
        h.feed_event(rich_event(7)).await;
        assert!(
            h.wait_finished(1, Duration::from_secs(5)).await,
            "応答生成が完了しない"
        );

        let prompts = SlowRunner::snapshot(&h.runner.system_prompts);
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert!(p.contains("npub1author"), "npub が載る: {p}");
        assert!(p.contains("0011223344556677"), "pubkey が載る: {p}");
        assert!(p.contains("note1target"), "note id が載る: {p}");
        assert!(p.contains("kind:7"), "kind が載る: {p}");
        assert!(p.contains("リアクション"), "種別ラベルが載る: {p}");
        assert!(
            p.contains("nostr_reply(target=\"note1target\")"),
            "従来の返信指示も残る: {p}"
        );
    }

    /// [#282] 転記（Discord webhook）とエージェント向けの記録は**同じ本文**を出す
    /// （転記にだけ kind が載る非対称の解消）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_and_agent_record_carry_the_same_information() {
        const URL: &str = "https://discord.com/api/webhooks/1/tok";
        let runner = SlowRunner::new(Duration::from_millis(1)).with_relay_target(URL);
        let h = Harness::with_runner("agent-parity", runner, 8, 32);

        h.feed_event(rich_event(1)).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        let relayed = SlowRunner::snapshot(&h.runner.relayed);
        assert_eq!(relayed.len(), 1);
        assert!(
            relayed[0].contains(&recorded[0]),
            "転記本文がエージェントの記録本文を丸ごと含む: relayed={} recorded={}",
            relayed[0],
            recorded[0]
        );
        for needle in ["npub1author", "note1target", "kind:1"] {
            assert!(
                relayed[0].contains(needle) && recorded[0].contains(needle),
                "{needle} が両方に載る"
            );
        }
    }

    /// [#570] トークン上限未満の受信は退避を**完全に素通り**する: 会話履歴へ残る本文は
    /// 生の `inbound_text` と 1 バイトも変わらず、ワークスペースに退避ファイルも作られない。
    /// 閾値以下 no-op の回帰防止。
    ///
    /// Nostr 受信（source=nostr / log_type=speech）の実測最大は **1,959 字 / 2,179 バイト**
    /// （288 行・2,000 字超は 0 行）。ここで使う ASCII 主体 **6,761 字**は実測最大ではなく、
    /// **no-op の回帰用に十分大きい合成値**（`o200k_base` で約 1,700 トークン < 2,500）。
    /// 純粋なかな 6,761 字は ~1 字/トークンで上限を超えてしまうため、「上限未満だが実測最大より
    /// 十分大きい」を作れる ASCII 主体（~4 字/トークン）で組む。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_below_limit_is_recorded_verbatim() {
        // no-op 回帰用の合成本文: ASCII 主体 6,761 字（≒ 1,700 トークン < 2,500）。
        // 実測の Nostr 受信最大（1,959 字）より十分大きく、かつ上限未満に収まる。
        let content: String = "the nostaro bot posts publicly. "
            .repeat(220)
            .chars()
            .take(6_761)
            .collect();
        assert_eq!(
            content.chars().count(),
            6_761,
            "合成本文の文字数がズレている"
        );
        let ev = event("prodmax", "0011223344556677", &content);
        // 前提: この本文はトークン上限未満（退避されない領域）。
        assert!(
            opencrab_core::tokens::estimate_tokens(&ev.inbound_text())
                < opencrab_actions::TOOL_RESULT_TOKEN_LIMIT,
            "前提が崩れている: 合成本文（ASCII 主体 6,761 字）が上限を超えた"
        );
        let expected = ev.inbound_text();

        // 退避先（ワークスペース）を与えても、閾値以下なら 1 件も書かれない。
        let dir = tempfile::tempdir().unwrap();
        let runner =
            SlowRunner::new(Duration::from_millis(1)).with_workspace_root(dir.path().into());
        let h = Harness::with_runner("agent-570-noop", runner, 8, 32);
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0], expected,
            "閾値以下の受信は生本文のまま記録される（no-op）"
        );
        // 発言者識別子（sender_id）は退避と無関係に不変（#501 の除外条件に巻き込まれない）。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_speakers),
            vec!["0011223344556677".to_string()],
            "speaker_id は退避経路で変わらない"
        );
        // 退避ファイルは作られない。
        let tmp = dir.path().join("tmp");
        let offloaded = tmp.exists()
            && tmp
                .read_dir()
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
        assert!(!offloaded, "閾値以下なのに退避ファイルが作られた");
    }

    /// [#570] 閾値を超える受信は、tool_result と同じ仕組みでワークスペースへ退避され、
    /// 会話履歴には生データを 1 バイトも含まないメタ案内だけが残る。退避ファイルは
    /// `<workspace>/tmp/` にあり（`ws_read` で読み返せる）、全文が入っている。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_over_limit_is_offloaded_to_workspace() {
        // 上限（2,500 トークン）を確実に超える本文。
        let content = "needle-token ".repeat(6_000);
        let ev = event("bigid", "aabbccddeeff0011", &content);
        let full = ev.inbound_text();
        assert!(
            opencrab_core::tokens::estimate_tokens(&full)
                >= opencrab_actions::TOOL_RESULT_TOKEN_LIMIT,
            "前提が崩れている: 上限を超えていない"
        );

        let dir = tempfile::tempdir().unwrap();
        let runner =
            SlowRunner::new(Duration::from_millis(1)).with_workspace_root(dir.path().into());
        let h = Harness::with_runner("agent-570-big", runner, 8, 32);
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        let text = &recorded[0];
        // 生データは 1 バイトも会話履歴へ入らない。
        assert!(
            !text.contains("needle-token"),
            "生データが会話履歴に混ざった: {text}"
        );
        // tool_result と同じ案内書式（退避先パス入り）。
        assert!(text.contains("withheld"), "案内書式が既存と違う: {text}");
        assert!(text.contains("tmp/"), "退避先パスが案内に無い: {text}");
        // speaker_id は不変。
        assert_eq!(
            SlowRunner::snapshot(&h.runner.recorded_speakers),
            vec!["aabbccddeeff0011".to_string()],
        );
        // 退避ファイルが 1 つでき、全文（生の inbound_text）が入っている。
        let tmp = dir.path().join("tmp");
        let files: Vec<_> = tmp.read_dir().unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(files.len(), 1, "退避ファイルが 1 件だけできる");
        let saved = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(
            saved, full,
            "退避ファイルに全文が入る（ws_read で読み返せる）"
        );
    }

    /// [#570] 退避先（workspace_root）が無い／解決できない場合でも、閾値超の生データを
    /// 会話履歴へ丸ごと入れない。「保存できず捨てた」と分かる案内だけを残す（fail-safe）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_over_limit_without_workspace_is_not_dumped_raw() {
        let content = "secret-body ".repeat(6_000);
        let ev = event("nows", "0011223344556677", &content);
        // workspace_root を仕込まない（= agent_workspace_root は None）。
        let h = Harness::new("agent-570-nows", Duration::from_millis(1), 8, 32);
        h.feed_event(ev).await;

        let recorded = SlowRunner::snapshot(&h.runner.recorded);
        assert_eq!(recorded.len(), 1);
        let text = &recorded[0];
        assert!(
            !text.contains("secret-body"),
            "退避先が無いのに生データが会話履歴へ流れた: {text}"
        );
        assert!(text.contains("could not be saved"), "{text}");
    }

    #[test]
    fn test_seen_events_dedup_and_eviction() {
        let mut seen = SeenEvents::new(2);
        assert!(seen.check_and_insert("a")); // 新規
        assert!(!seen.check_and_insert("a")); // 既知
        assert!(seen.check_and_insert("b"));
        // cap=2 を超えると最古（a）を追い出す。
        assert!(seen.check_and_insert("c"));
        // a は追い出されたので再び新規扱い（replay 耐性は cap ぶん）。
        assert!(seen.check_and_insert("a"));
        // b はまだ保持（直近）… ではなく a 追加で b が最古になり追い出される可能性。
        // 少なくとも直近の c は既知のまま。
        assert!(!seen.check_and_insert("c"));
    }

