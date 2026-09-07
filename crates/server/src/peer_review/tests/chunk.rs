    #[test]
    fn long_japanese_content_chunks_losslessly() {
        let content = "日本語のレビュー対象コンテンツ。".repeat(300); // 4800 chars
        let header = PeerReviewHeader {
            agent_name: "a",
            task: None,
            instructions: None,
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, &content, CHUNK_LIMIT);
        let parts = &msgs[1..];
        assert!(parts.len() >= 3);
        // 各チャンクは limit + "part X/N\n" プレフィクス以内
        for (i, p) in parts.iter().enumerate() {
            let prefix = format!("part {}/{}\n", i + 1, parts.len());
            assert!(p.starts_with(&prefix));
            let body = &p[prefix.len()..];
            assert!(body.chars().count() <= CHUNK_LIMIT);
        }
        // 結合で原文復元（要約・切り詰めが無い）
        let reassembled: String = parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let prefix = format!("part {}/{}\n", i + 1, parts.len());
                p[prefix.len()..].to_string()
            })
            .collect();
        assert_eq!(reassembled, content);
        // ヘッダの parts 数が一致
        assert!(msgs[0].contains(&format!("parts: {}", parts.len())));
    }

