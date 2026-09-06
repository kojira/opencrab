    #[test]
    fn test_build_router_empty_keys() {
        let config = LlmConfig::default();
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().is_empty());
    }

    #[test]
    fn test_build_router_with_openrouter() {
        let mut providers = HashMap::new();
        providers.insert(
            "openrouter".to_string(),
            ProviderConfig {
                api_key: "sk-test-key".to_string(),
                app_name: "TestApp".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "openrouter".to_string(),
            ..Default::default()
        };
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().contains(&"openrouter"));
    }

    /// #660 罠1（片方だけ改名）: ルーティングキーは **セクションキー（名乗り名）** で
    /// 決まる。`type` が形式を選び、名乗り名は別。`hermit`（type=openai）は "hermit" で
    /// 登録され、形式名 "openai" では引けない。
    #[test]
    fn provider_registers_under_section_key_not_type() {
        let mut providers = HashMap::new();
        providers.insert(
            "hermit".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: "dummy".to_string(),
                base_url: "http://localhost:8765/v1".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "hermit".to_string(),
            ..Default::default()
        };
        let router = build_llm_router(&config).unwrap();
        assert!(
            router.provider_names().contains(&"hermit"),
            "セクションキー hermit で登録されるべき"
        );
        assert!(
            !router.provider_names().contains(&"openai"),
            "形式名 openai では登録されないべき（二重命名を作らない）"
        );
    }

    /// #660 罠1: `type` 省略時はセクションキーを形式名として使う。既存の
    /// `[llm.providers.openai]`（type 無し）は無編集で従来どおり openai 形式・openai 名で動く。
    #[test]
    fn omitted_type_defaults_to_section_key() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: "dummy".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "openai".to_string(),
            ..Default::default()
        };
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().contains(&"openai"));
    }

    /// #660 罠1（無言スキップの再発防止）: 未知の `type` は起動を止める（hard error）。
    /// 旧実装のように `None` で黙ってスキップしたらこのテストが赤くなる。
    #[test]
    fn unknown_provider_type_is_hard_error() {
        let mut providers = HashMap::new();
        providers.insert(
            "weird".to_string(),
            ProviderConfig {
                provider_type: "nonexistent".to_string(),
                api_key: "x".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "weird".to_string(),
            ..Default::default()
        };
        assert!(
            build_llm_router(&config).is_err(),
            "未知 type は起動失敗にすべき（無言スキップは欠陥）"
        );
    }

    /// #660 bonsai 回帰: bonsai 専用アームを消し `type="llamacpp"` の一般機構へ寄せた。
    /// 名乗り名 "bonsai" で登録され、形式名 "llamacpp" では引けないこと（起動不能にしない）。
    #[test]
    fn bonsai_type_llamacpp_registers_under_bonsai() {
        let mut providers = HashMap::new();
        providers.insert(
            "bonsai".to_string(),
            ProviderConfig {
                provider_type: "llamacpp".to_string(),
                base_url: "http://localhost:8081".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "bonsai".to_string(),
            ..Default::default()
        };
        let router = build_llm_router(&config).unwrap();
        assert!(
            router.provider_names().contains(&"bonsai"),
            "bonsai は名乗り名 bonsai で登録されるべき"
        );
        assert!(
            !router.provider_names().contains(&"llamacpp"),
            "形式名 llamacpp では登録されないべき"
        );
    }

    /// #660: fallback.chain が定義されていない provider を指したら起動を止める。
    #[test]
    fn fallback_chain_undefined_provider_is_hard_error() {
        let mut providers = HashMap::new();
        providers.insert(
            "hermit".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: "dummy".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "hermit".to_string(),
            fallback: FallbackConfig {
                chain: vec!["ghost".to_string()],
            },
            ..Default::default()
        };
        assert!(
            build_llm_router(&config).is_err(),
            "chain の未定義 provider は起動失敗にすべき"
        );
    }

    /// #660: alias が定義されていない provider を指したら起動を止める
    /// （rename の取りこぼしを黙って通さない）。
    #[test]
    fn alias_undefined_provider_is_hard_error() {
        let mut providers = HashMap::new();
        providers.insert(
            "hermit".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: "dummy".to_string(),
                ..Default::default()
            },
        );
        let mut aliases = HashMap::new();
        aliases.insert(
            "smart".to_string(),
            AliasConfig {
                provider: "openai".to_string(), // 改名し忘れ（hermit にすべき）を模す
                model: "claude-sonnet-4-6".to_string(),
            },
        );
        let config = LlmConfig {
            providers,
            aliases,
            default_provider: "hermit".to_string(),
            ..Default::default()
        };
        assert!(
            build_llm_router(&config).is_err(),
            "alias の未定義 provider（改名取りこぼし）は起動失敗にすべき"
        );
    }

    /// #660: 配布する `config/default.toml` が実際に router を組めること。
    /// hermit / bonsai が名乗り名で登録され、openai 名は消えていること。type / aliases /
    /// fallback.chain の整合まで含めて配布物を end-to-end で固定する。
    #[test]
    fn shipped_default_toml_builds_router_with_hermit() {
        let _guard = env_lock();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/default.toml");
        let config = load_config(path).expect("shipped default.toml must load");
        let router =
            build_llm_router(&config.llm).expect("shipped default.toml must build a router");
        let names = router.provider_names();
        assert!(
            names.contains(&"hermit"),
            "hermit が登録されていない: {names:?}"
        );
        assert!(
            names.contains(&"bonsai"),
            "bonsai が登録されていない: {names:?}"
        );
        assert!(
            !names.contains(&"openai"),
            "openai 名は消えているべき: {names:?}"
        );
    }

    /// #660: default_provider が定義されていなければ起動を止める（chain / alias と対称）。
    /// bare model の解決先という LIVE な参照で、rename 取りこぼしを黙って通さない。
    #[test]
    fn default_provider_undefined_is_hard_error() {
        let mut providers = HashMap::new();
        providers.insert(
            "hermit".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: "dummy".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "openai".to_string(), // 改名し忘れ（hermit にすべき）を模す
            ..Default::default()
        };
        assert!(
            build_llm_router(&config).is_err(),
            "定義されていない default_provider は起動失敗にすべき"
        );
    }

    /// #660: `KNOWN_PROVIDER_TYPES` の全 type が実際に match アームへ振り分く（＝ dispatch
    /// できる）こと。const に足したのに build_llm_router へアームを足し忘れたら、その type は
    /// 未知として bail し、このテストが赤くなる（const とアームの drift 防止）。
    #[test]
    fn every_known_provider_type_dispatches() {
        for &ty in KNOWN_PROVIDER_TYPES {
            let mut providers = HashMap::new();
            providers.insert(
                "p".to_string(),
                ProviderConfig {
                    provider_type: ty.to_string(),
                    // api_key が要る形式（openai/anthropic/google/openrouter）でも登録される
                    // よう埋める。要らない形式は無視するだけ。
                    api_key: "x".to_string(),
                    ..Default::default()
                },
            );
            let config = LlmConfig {
                providers,
                default_provider: "p".to_string(),
                ..Default::default()
            };
            let router = build_llm_router(&config)
                .unwrap_or_else(|e| panic!("type '{ty}' が dispatch できない: {e}"));
            assert!(
                router.provider_names().contains(&"p"),
                "type '{ty}' はセクションキー p で登録されるべき"
            );
        }
    }

    /// #660: dashboard から **既定 provider を enabled=false** にする操作は reload で
    /// hard error になる（意図した挙動変化）。他 provider が残るのに既定名だけが消えると、
    /// bare model 要求が黙って fallback へ流れる——本 PR が塞いだ状態の実行時再構成なので、
    /// 500 + ロールバックで止める。既存の e2e（唯一 provider の無効化で providers が空になり
    /// 「未設定」側へ落ちるケース）は踏めていない経路なので、ここで固定する。
    #[test]
    fn disabling_the_default_provider_via_override_is_hard_error() {
        use opencrab_db::queries::LlmProviderOverrideRow;
        let mut providers = HashMap::new();
        providers.insert(
            "hermit".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: "dummy".to_string(),
                ..Default::default()
            },
        );
        // 既定以外に残る provider（ローカルなので常に登録される）。
        providers.insert("ollama".to_string(), ProviderConfig::default());
        let base = LlmConfig {
            providers,
            default_provider: "hermit".to_string(),
            ..Default::default()
        };
        // base は default=hermit が定義済みなので通る。
        assert!(build_llm_router(&base).is_ok());

        // hermit を無効化 → 実効設定から hermit セクションが消え、ollama は残る。
        let overrides = vec![LlmProviderOverrideRow {
            provider: "hermit".to_string(),
            enabled: Some(false),
            ..Default::default()
        }];
        let merged = apply_llm_overrides(&base, &overrides);
        assert!(!merged.providers.contains_key("hermit"));
        assert!(merged.providers.contains_key("ollama"));

        // default=hermit が宙に浮くので reload（build_llm_router）は Err。
        assert!(
            build_llm_router(&merged).is_err(),
            "既定 provider の無効化は reload で hard error にすべき"
        );
    }

    /// #457: 凝縮ラン（記憶の 3 段目）の**出荷時既定は ON**。
    ///
    /// `[memory_condense]` を書かない設定でも `enabled` が既定 true になることを、実際の設定
    /// ロード経路と同じ `AppConfig` の serde 既定で固定する。`default_mc_enabled()` を false に
    /// 戻すと落ちる（恒真テストにしないため enabled を直接 assert する）。
    /// 間隔・窓幅・timeout の仮値（#411 PR-3 の領分）も本 PR で不変であることを併せて固定する。
    #[test]
    fn memory_condense_ships_enabled_by_default() {
        let cfg: AppConfig = toml::from_str("").expect("empty config must parse");
        assert!(
            cfg.memory_condense.enabled,
            "[memory_condense] 省略時も enabled は出荷時既定 true であるべき（#457）"
        );
        // #411 PR-3 で実測確定する仮値。本 PR では変えない。
        assert_eq!(cfg.memory_condense.min_new_units, 20);
        assert_eq!(cfg.memory_condense.min_interval_minutes, 10080);
        assert_eq!(cfg.memory_condense.timeout_secs, 600);
    }
