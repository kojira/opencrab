    #[test]
    fn test_watch_command_includes_relays_and_filters() {
        let cli = NostaroCli::new();
        let config = NostrConfig {
            relays: vec![],
            filter: crate::config::NostrFilter {
                authors: vec!["npub1abc".to_string()],
                keywords: vec!["opencrab".to_string()],
                kinds: vec![],
            },
        };
        let cmd = cli.build_watch_command("agent-1", &config).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // 既定リレー2つが = 形式のフラグで渡る（config の default に依存しない）。
        assert!(args.contains(&"--relay=wss://yabu.me".to_string()));
        assert!(args.contains(&"--relay=wss://r.kojira.io".to_string()));
        assert!(args.contains(&"--author=npub1abc".to_string()));
        assert!(args.contains(&"--keyword=opencrab".to_string()));
        // kind 未指定 → 既定 1。
        assert!(args.contains(&"--kind=1".to_string()));
        assert!(args.contains(&"--json".to_string()));
        // 条件の結合は OR を明示する（#278）。
        assert!(args.contains(&"--match=any".to_string()));
        // per-agent config が渡る。
        assert!(args
            .iter()
            .any(|a| a.contains("data/agents/agent-1/nostr/config.toml")));
    }

    /// [#278] 受信セマンティクス（条件の結合方法）は **argv に明示**する。
    ///
    /// nostaro の `--match` 既定は `any` だが、既定に寄りかかると nostaro 側が既定を
    /// 変えた瞬間に opencrab の受信が黙って変わる（#278 が起きた原因そのもの）。
    /// フィルタが空でも `--match=any` が 1 つだけ乗り、`--match=all` は決して乗らない。
    #[test]
    fn test_watch_command_pins_match_mode_to_any() {
        let cli = NostaroCli::new();
        let cmd = cli
            .build_watch_command("agent-1", &NostrConfig::default())
            .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args.iter().filter(|a| a.starts_with("--match")).count(),
            1,
            "--match は 1 回だけ渡す: {args:?}"
        );
        assert!(args.contains(&"--match=any".to_string()), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "--match=all"),
            "AND 結合（--match=all）にしない: {args:?}"
        );
    }

    /// [#271/#278] `--no-mention-only` は**絶対に渡さない**。
    ///
    /// nostaro の mention-only 既定（自分宛の p タグを購読）が、フィルタ未指定でも
    /// 購読を「自分宛のみ」に閉じる唯一の仕組み。ここでそれを切ると全ノート購読
    /// （firehose）になる。フィルタの有無に関わらず渡らないことを固定する。
    #[test]
    fn test_watch_command_never_disables_mention_only() {
        let cli = NostaroCli::new();
        let configs = [
            NostrConfig::default(),
            NostrConfig {
                relays: vec![],
                filter: crate::config::NostrFilter {
                    authors: vec!["npub1abc".to_string()],
                    keywords: vec!["opencrab".to_string()],
                    kinds: vec![1, 7],
                },
            },
        ];
        for config in configs {
            let cmd = cli.build_watch_command("agent-1", &config).unwrap();
            let args: Vec<String> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert!(
                !args.iter().any(|a| a.starts_with("--no-mention-only")),
                "mention-only を切ると自分宛以外も流れ込む: {args:?}"
            );
            // `--mention-only` も渡さない（nostaro の既定 true に委ねる。明示すると
            // 将来 `--no-mention-only` と併記したときにパースエラーになる）。
            assert!(
                !args.iter().any(|a| a.starts_with("--mention-only")),
                "mention-only は nostaro の既定に委ねる: {args:?}"
            );
        }
    }

    /// [#271] 自動採用（bootstrap）した設定では `--keyword` が 1 つも乗らない。
    ///
    /// #264 の自己ブートストラップが `keywords=[自分の npub]` を自動設定していたため、
    /// 本文に npub 文字列を含まない e/p タグだけの返信が keyword 条件で落ちていた。
    /// 空フィルタなら keyword フラグは組み立てられない（＝nostaro の mention-only
    /// 既定だけが効く）ことを argv で固定する。
    #[test]
    fn test_watch_command_has_no_keyword_when_filter_is_empty() {
        let cli = NostaroCli::new();
        let cmd = cli
            .build_watch_command("agent-1", &NostrConfig::default())
            .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !args.iter().any(|a| a.starts_with("--keyword")),
            "自動設定の keyword は乗せない: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--author")),
            "自動設定の author は乗せない: {args:?}"
        );
        // 絞り込みが無くても watch は張る（mention-only 既定で「自分宛のみ」）。
        assert!(args.contains(&"--kind=1".to_string()), "{args:?}");
    }

