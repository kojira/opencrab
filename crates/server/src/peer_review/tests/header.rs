    // ---- ヘッダ組み立て ----

    #[test]
    fn header_includes_task_and_instructions() {
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: Some((12, "ship feature", Some("tests green"))),
            instructions: Some("check the error handling"),
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, "diff content", CHUNK_LIMIT);
        assert_eq!(msgs.len(), 2);
        let head = &msgs[0];
        assert!(head.starts_with("[Peer Review Request] from crab-a — task #12"));
        assert!(head.contains("goal: ship feature"));
        assert!(head.contains("contract: tests green"));
        assert!(head.contains("instructions: check the error handling"));
        assert!(head.contains("score: <0.0-1.0>"));
        assert!(head.contains("parts: 1"));
        assert_eq!(msgs[1], "part 1/1\ndiff content");
    }

    #[test]
    fn header_without_task_or_contract() {
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: None,
            instructions: None,
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
        assert!(msgs[0].contains("no active task"));
        assert!(!msgs[0].contains("goal:"));
        assert!(!msgs[0].contains("instructions:"));

        // contract が空文字列なら contract 行は出ない
        let header = PeerReviewHeader {
            agent_name: "crab-a",
            task: Some((3, "g", Some("  "))),
            instructions: None,
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
        assert!(msgs[0].contains("task #3"));
        assert!(msgs[0].contains("goal: g"));
        assert!(!msgs[0].contains("contract:"));
    }

    #[test]
    fn header_includes_mention_after_marker() {
        let header = PeerReviewHeader {
            agent_name: "a",
            task: None,
            instructions: None,
            mention: Some("<@1234567890>"),
        };
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
        // starts-with 判定を壊さないよう、メンションは marker の後ろ
        assert!(msgs[0].starts_with("[Peer Review Request] <@1234567890> from a"));
    }

    #[test]
    fn header_stays_within_discord_limit_with_long_fields() {
        // goal 2000 / contract 4000 / instructions 無制限でもヘッダは1通(2000 chars)に収まる
        let goal = "g".repeat(2000);
        let contract = "c".repeat(4000);
        let instructions = "i".repeat(5000);
        let header = PeerReviewHeader {
            agent_name: "very-long-agent-name-agent",
            task: Some((99, goal.as_str(), Some(contract.as_str()))),
            instructions: Some(instructions.as_str()),
            mention: None,
        };
        let msgs = build_peer_review_messages(&header, "x", CHUNK_LIMIT);
        assert!(
            msgs[0].chars().count() <= 2000,
            "header must fit one Discord message, got {}",
            msgs[0].chars().count()
        );
        // 切り詰めが起きていることの確認
        assert!(msgs[0].contains("…"));
    }

