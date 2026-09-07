    #[test]
    fn format_progress_bounds_and_labels() {
        let v = PeerReviewVerdict {
            score: Some(0.4),
            gaps: vec!["a".repeat(600), "b".to_string()],
            summary: "needs work".to_string(),
        };
        let s = format_peer_review_progress(&v, "crab-b");
        assert!(s.starts_with("[peer review] score 0.40 (from crab-b): needs work"));
        assert!(s.chars().count() < 1300, "progress entry must stay bounded");

        let v = PeerReviewVerdict {
            score: None,
            gaps: vec![],
            summary: "s".to_string(),
        };
        assert!(format_peer_review_progress(&v, "r").contains("score n/a"));
    }

