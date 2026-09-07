    #[test]
    fn test_apply_llm_overrides() {
        use opencrab_db::queries::LlmProviderOverrideRow;
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: "toml-key".to_string(),
                base_url: "https://toml.example".to_string(),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key: "ant-key".to_string(),
                ..Default::default()
            },
        );
        let base = LlmConfig {
            providers,
            ..toml::from_str("").unwrap()
        };

        let overrides = vec![
            // openai: キーだけ DB 側で差し替え
            LlmProviderOverrideRow {
                provider: "openai".to_string(),
                api_key: Some("db-key".to_string()),
                ..Default::default()
            },
            // anthropic: 強制無効
            LlmProviderOverrideRow {
                provider: "anthropic".to_string(),
                enabled: Some(false),
                ..Default::default()
            },
            // ollama: TOML に無いが UI から有効化（base_url のみ）
            LlmProviderOverrideRow {
                provider: "ollama".to_string(),
                enabled: Some(true),
                base_url: Some("http://localhost:11434".to_string()),
                ..Default::default()
            },
        ];

        let merged = apply_llm_overrides(&base, &overrides);
        assert_eq!(merged.providers["openai"].api_key, "db-key");
        // 上書きしていないフィールドは TOML 値を維持
        assert_eq!(merged.providers["openai"].base_url, "https://toml.example");
        assert!(
            !merged.providers.contains_key("anthropic"),
            "disabled provider must be removed"
        );
        assert_eq!(
            merged.providers["ollama"].base_url,
            "http://localhost:11434"
        );
    }

    #[test]
    fn test_apply_llm_overrides_empty_is_identity() {
        let base: LlmConfig = toml::from_str("").unwrap();
        let merged = apply_llm_overrides(&base, &[]);
        assert_eq!(merged.providers.len(), base.providers.len());
        assert_eq!(merged.default_provider, base.default_provider);
    }

