    // ---- 返信の回収（#156 S4 で Discord gateway から移設。解析 / 整形 / 3 つのゲート）

    /// 回収の便宜関数（移設前の 6 引数の呼び出し形をテスト内で保つ）。
    fn record_discord_reply(
        db: &opencrab_db::Db,
        agent_id: &str,
        session_id: &str,
        sender_id: &str,
        sender_name: &str,
        text: &str,
    ) -> bool {
        record_peer_review_reply(
            db,
            opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
            agent_id,
            session_id,
            sender_id,
            sender_name,
            text,
        )
    }

    #[test]
    fn parse_reply_full_form() {
        let v = parse_peer_review_reply(
            "[Peer Review] score: 0.75\ngaps:\n- tests not run\n- no error handling\nsummary: solid but unverified",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.75));
        assert_eq!(v.gaps, vec!["tests not run", "no error handling"]);
        assert_eq!(v.summary, "solid but unverified");
    }

    #[test]
    fn parse_reply_inline_and_variants() {
        // インライン gaps、score にスラッシュ形式、大文字
        let v = parse_peer_review_reply(
            "  [Peer Review] Score: 0.9/1.0, Gaps: none, Summary: looks good",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.9));
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "looks good");

        // score 欠落 → None、summary 欠落 → 本文フォールバック
        let v = parse_peer_review_reply("[Peer Review] this looks fine to me").unwrap();
        assert_eq!(v.score, None);
        assert!(v.summary.contains("looks fine"));

        // 1.0 超は clamp
        let v = parse_peer_review_reply("[Peer Review] score: 8.5 summary: s").unwrap();
        assert_eq!(v.score, Some(1.0));
    }

    #[test]
    fn parse_reply_rejects_non_marker() {
        assert!(parse_peer_review_reply("just chatting about [Peer Review] stuff").is_none());
        assert!(parse_peer_review_reply("[Peer Review Request] from a").is_none());
    }

    /// **依頼側と回収側の目印が噛み合っていること**（#157 の依頼側 / #156 S4 の回収側）。
    ///
    /// 依頼が投稿する本文の先頭は [`PEER_REVIEW_REQUEST_MARKER`]、回収が探すのは
    /// [`PEER_REVIEW_REPLY_MARKER`]。前者が後者で始まってしまうと、依頼メッセージ自体が
    /// 「返信」として回収され、依頼した瞬間に verdict が捏造される。
    #[test]
    fn request_marker_is_not_harvested_as_a_reply() {
        assert!(!PEER_REVIEW_REQUEST_MARKER.starts_with(PEER_REVIEW_REPLY_MARKER));
        // 依頼側が実際に組み立てるヘッダ（`post_peer_review` と同じ形）でも回収されない。
        let header = format!("{PEER_REVIEW_REQUEST_MARKER}<@42> from crab-a — task #7\n");
        assert!(parse_peer_review_reply(&header).is_none());
        // 逆向き: 回収が探す目印は依頼側の目印の前置ではない（未回収判定も同様に
        // `[peer review requested]` が `[peer review]` で始まらないことに依存する）。
        assert!(!"[peer review requested] posted".starts_with("[peer review]"));
    }

    #[test]
    fn parse_reply_finds_marker_after_preamble_and_markdown() {
        // debounce がレビュアーの前置きと verdict を結合するケース
        let v = parse_peer_review_reply(
            "Looking at it now.\n[Peer Review] score: 0.8, gaps: none, summary: fine",
        )
        .unwrap();
        assert_eq!(v.score, Some(0.8));

        // markdown 装飾付き行頭
        let v = parse_peer_review_reply("**[Peer Review]** score: 0.5 summary: hm").unwrap();
        assert_eq!(v.score, Some(0.5));

        // 行の途中の言及は依然として無視
        assert!(parse_peer_review_reply("the diff mentions [Peer Review] in prose").is_none());

        // 本文中の "gaps" という単語をフィールド開始と誤認しない（コロン必須）
        let v = parse_peer_review_reply("[Peer Review] score: 1.0 summary: no gaps found").unwrap();
        assert!(v.gaps.is_empty());
        assert_eq!(v.summary, "no gaps found");
    }

