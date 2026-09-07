    // ---- #319: 受信ターンの呼び出し元は発言者から決まる ----

    /// ダミー鍵（実在の pubkey は書かない）。
    const OWNER_PK: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const STRANGER_PK: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    /// **本丸**: オーナーの pubkey から届いた受信ターンは `Owner` で走る。
    ///
    /// 以前は応答生成側が `CallerIdentity::Agent` 固定で、OWNER_ONLY / TRUSTED_ONLY の
    /// ツールが list にも dispatch にも出なかった（#319）。
    #[tokio::test]
    async fn inbound_from_owner_runs_as_owner() {
        let h = Harness::with_runner(
            "agent-caller",
            SlowRunner::new(Duration::from_millis(0)).with_owner_pubkey(OWNER_PK),
            4,
            8,
        );
        h.feed("evt-owner", OWNER_PK, "設定を変えて").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);

        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Owner],
            "オーナー発の受信ターンが Owner で走っていない"
        );
        // 解決には**受信イベントの pubkey** を渡している（session_id ではない）。
        assert_eq!(
            h.runner.caller_queries.lock().unwrap().as_slice(),
            [OWNER_PK.to_string()]
        );
    }

    /// **本丸**: owner でない発言者から届いたターンは `Agent` のまま（昇格しない）。
    ///
    /// #698 以降、入り口に立てる（＝ターンを起こせる）のは許可源（フォロイー ∪ owner ∪
    /// co_agent ∪ trusted_users）だけなので、ここでの「他人」は**フォロイーだが owner では
    /// ない**相手（`feed` が followees へ入れる）。許可源の外にいる完全な他人は
    /// [`unallowed_author_event_is_dropped_before_store`] が別に見る。
    #[tokio::test]
    async fn inbound_from_stranger_stays_agent() {
        let h = Harness::with_runner(
            "agent-caller",
            SlowRunner::new(Duration::from_millis(0)).with_owner_pubkey(OWNER_PK),
            4,
            8,
        );
        h.feed("evt-stranger", STRANGER_PK, "設定を変えて").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);

        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Agent],
            "他人の pubkey が昇格した"
        );
    }

    /// オーナー未設定なら誰も Owner にならない（fail-closed）。
    #[tokio::test]
    async fn inbound_without_configured_owner_stays_agent() {
        // with_owner_pubkey を仕込まない＝解決側がオーナー無しと答える。
        let h = Harness::new("agent-caller", Duration::from_millis(0), 4, 8);
        h.feed("evt-1", OWNER_PK, "設定を変えて").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);

        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Agent]
        );
    }

    // ---- #698: フォロイー ∪ owner ∪ co_agent ∪ trusted_users 以外は着火も記録もさせない元栓 ----

    /// **本丸**: どの許可源にも属さない作者のイベントは、**record より前に**捨てる。
    ///
    /// 会話履歴に 1 件も入らず（`recorded` が空）、応答生成も走らない（`finished` が 0）。
    /// 捨てた数は揮発カウンタに乗る（`dropped == 1`）。さらに **`resolve_nostr_caller` が
    /// 呼ばれない**（ドロップは純メモリで、DB 往復がホットパスに乗らない / #698 req3）。
    ///
    /// **変異確認**: `handle_event` の元栓 `if !allow.is_allowed(&author_key) { return; }` を
    /// 外すと、未許可作者が記録され応答生成まで走る（このテストが赤くなる）。
    #[tokio::test]
    async fn unallowed_author_event_is_dropped_before_store() {
        // owner 未設定・許可集合空。feed_unfollowed_event は followees へ入れない。
        let h = Harness::new("agent-gate", Duration::from_millis(0), 4, 8);
        h.feed_unfollowed_event(event("evt-x", STRANGER_PK, "無視されるはず"))
            .await;

        // record より前で捨てているので、少し待っても何も起きない。
        assert!(
            !h.wait_finished(1, Duration::from_millis(200)).await,
            "未許可作者のイベントで応答生成が走った（元栓が効いていない）"
        );
        assert!(
            h.runner.recorded.lock().unwrap().is_empty(),
            "未許可作者のイベントが会話履歴に記録された（store 前で捨てていない）"
        );
        assert_eq!(h.dropped(), 1, "揮発カウンタが増えていない");
        // ホットパスは純メモリ: ドロップ前に resolve_nostr_caller（DB 往復）を呼ばない。
        assert!(
            h.runner.caller_queries.lock().unwrap().is_empty(),
            "ドロップ判定の前に resolve_nostr_caller が呼ばれた（ホットパスに DB 往復が乗っている）"
        );
    }

    /// フォロイー（kind:3）の作者は通す（記録され応答生成が走る）。owner でなくても入り口に
    /// 立てる（#698 の許可集合はフォロイー ∪ owner ∪ co_agent ∪ trusted_users）。
    #[tokio::test]
    async fn followee_event_passes_the_gate() {
        let h = Harness::new("agent-gate", Duration::from_millis(0), 4, 8);
        // feed は発言者を followees へ入れてから流す＝フォロイー扱い。
        h.feed("evt-f", STRANGER_PK, "フォローしている相手").await;
        assert!(h.wait_finished(1, Duration::from_secs(2)).await);
        assert_eq!(
            h.runner.recorded.lock().unwrap().len(),
            1,
            "フォロイーのイベントが記録されていない"
        );
        assert_eq!(h.dropped(), 0, "フォロイーを誤って捨てた");
    }

    /// **本丸**: owner の作者はフォロイーでなくても通す（元栓を素通り）。owner は許可源
    /// `AllowSources::owner`（`nostr_gate_allow_keys` 由来）に載る。
    ///
    /// **変異確認**: 許可源から owner を落とすと、フォローしていない owner が捨てられる。
    #[tokio::test]
    async fn owner_event_bypasses_gate_even_when_not_followed() {
        // owner を仕込むが followees へは入れない（feed_unfollowed_event を使う）。
        let h = Harness::with_runner(
            "agent-gate",
            SlowRunner::new(Duration::from_millis(0)).with_owner_pubkey(OWNER_PK),
            4,
            8,
        );
        h.feed_unfollowed_event(event("evt-o", OWNER_PK, "オーナー発"))
            .await;
        assert!(
            h.wait_finished(1, Duration::from_secs(2)).await,
            "owner のイベントが元栓で捨てられた"
        );
        assert_eq!(h.runner.recorded.lock().unwrap().len(), 1);
        assert_eq!(h.dropped(), 0, "owner を捨てた");
        assert_eq!(
            h.runner.callers.lock().unwrap().as_slice(),
            [CallerIdentity::Owner],
            "owner 発のターンが Owner で走っていない"
        );
    }

    /// **裁定**: owner が明示登録した trusted_user（platform=nostr）はフォロイーでなくても
    /// 通す（締め出さない / #698 レビュー裁定）。
    ///
    /// **変異確認**: 許可源から trusted_users を落とすと、このテストが赤くなる。
    #[tokio::test]
    async fn trusted_user_event_passes_the_gate() {
        let h = Harness::with_runner(
            "agent-gate",
            SlowRunner::new(Duration::from_millis(0)).with_trusted_pubkey(STRANGER_PK),
            4,
            8,
        );
        h.feed_unfollowed_event(event("evt-t", STRANGER_PK, "信頼済みユーザー"))
            .await;
        assert!(
            h.wait_finished(1, Duration::from_secs(2)).await,
            "trusted_user のイベントが元栓で捨てられた"
        );
        assert_eq!(h.runner.recorded.lock().unwrap().len(), 1);
        assert_eq!(h.dropped(), 0, "trusted_user を捨てた");
    }

    /// co_agent（owner 等価）はフォロイーでなくても通す（エージェント間協働を壊さない / #485）。
    ///
    /// **変異確認**: 許可源から co_agents を落とすと、このテストが赤くなる。
    #[tokio::test]
    async fn co_agent_event_passes_the_gate() {
        let h = Harness::with_runner(
            "agent-gate",
            SlowRunner::new(Duration::from_millis(0)).with_co_agent_pubkey(STRANGER_PK),
            4,
            8,
        );
        h.feed_unfollowed_event(event("evt-c", STRANGER_PK, "協働エージェント"))
            .await;
        assert!(
            h.wait_finished(1, Duration::from_secs(2)).await,
            "co_agent のイベントが元栓で捨てられた"
        );
        assert_eq!(h.runner.recorded.lock().unwrap().len(), 1);
        assert_eq!(h.dropped(), 0, "co_agent を捨てた");
    }

    /// **本丸（3巡目レビュー）**: 更新時に DB 由来の許可源が読めない（DB 故障）とき、
    /// `build_allow_sources` が `Err` になり **前回の allow セルが保持される**（owner/trusted が
    /// 無音でキャッシュから消えない）。fetch_following（relay）側は fake で空リスト成功させ、
    /// **DB 側だけ**失敗させて「DB 部分失敗が `Ok(空)` に化けない」ことを固定する。
    ///
    /// **変異確認**: `nostr_gate_allow_keys` の `?` を `.unwrap_or_default()` に戻すと、DB エラーが
    /// `Ok(空)` になって owner が消え、このテストが赤くなる。
    #[tokio::test]
    async fn allow_refresh_keeps_previous_on_db_error() {
        // fetch_following は fake nostaro で空の following（成功）。DB 由来の許可源だけ Err。
        let agent = "agent-allow-db-error";
        let (_fake, cli) = fake_nostaro("selfpubkeyhex");
        let runner = SlowRunner::new(Duration::from_millis(0)).with_allow_keys_error();
        // 前回の許可集合（owner を持っている状態）を用意。
        let mut prev = AllowSources::default();
        prev.owner.insert(crate::pubkey::follow_key(OWNER_PK));
        let allow: AllowGate = Arc::new(RwLock::new(prev));

        refresh_allow_once(&runner, &cli, agent, &allow, &AllowSetStore::default()).await;

        // DB エラーで build_allow_sources が Err → セルは前回のまま（owner が消えていない）。
        assert!(
            allow
                .read()
                .unwrap()
                .owner
                .contains(&crate::pubkey::follow_key(OWNER_PK)),
            "DB エラーで前回の許可集合（owner）が消えた（DB 部分失敗が Ok(空) に化けている）"
        );
        let _ = std::fs::remove_dir_all(NostaroCli::agent_nostr_dir(agent).unwrap());
    }

