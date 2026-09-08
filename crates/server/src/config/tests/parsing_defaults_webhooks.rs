    #[test]
    fn test_voice_config_parses() {
        let toml_str = r#"
[voice]
enabled = true

[voice.stt]
provider = "openai"
language = "ja"

[voice.tts]
provider = "voicevox"
default_voice = "3"

[voice.tts.agent_voices]
crab = "3"
agent-b = "1"
"#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("voice config must parse");
        assert!(cfg.voice.enabled);
        assert_eq!(cfg.voice.stt.provider, "openai");
        assert_eq!(cfg.voice.stt.language.as_deref(), Some("ja"));
        assert_eq!(cfg.voice.tts.voice_for_agent("crab"), "3");
        assert_eq!(cfg.voice.tts.voice_for_agent("agent-b"), "1");
        assert_eq!(cfg.voice.tts.voice_for_agent("unknown"), "3");
    }

    #[test]
    fn test_voice_disabled_by_default() {
        let cfg: AppConfig = toml::from_str("").expect("empty config must parse");
        assert!(!cfg.voice.enabled);
    }

    #[test]
    fn test_default_config() {
        let toml_str = "";
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.database.path, "data/opencrab.db");
        assert_eq!(config.gateway.rest.port, 8080);
        assert_eq!(config.llm.default_provider, "openai");
    }

    #[test]
    fn conversation_config_typed_history_defaults_off_and_parses_enabled() {
        let default: AppConfig = toml::from_str("").expect("empty config must parse");
        assert!(!default.conversation.typed_history);
        assert!(!default.conversation.drop_response_directive);

        let enabled: AppConfig = toml::from_str(
            r#"
[conversation]
typed_history = true
"#,
        )
        .expect("conversation config must parse");
        assert!(enabled.conversation.typed_history);
        assert!(!enabled.conversation.drop_response_directive);
    }

    /// Regression guard for #149: shipping `config/default.toml` must keep the
    /// codex sandbox at `read-only` so the codex CLI cannot write to the
    /// workspace / run arbitrary builds. If someone flips it back to
    /// `danger-full-access` this test fails.
    #[test]
    fn test_default_toml_codex_sandbox_is_read_only() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/default.toml");
        let config = load_config(path).expect("shipped default.toml must load");
        let codex = config
            .llm
            .providers
            .get("codex")
            .expect("default.toml must define a codex provider");
        assert_eq!(
            codex.sandbox, "read-only",
            "codex sandbox in config/default.toml must stay read-only (regression #149)"
        );
    }

    // ---- #157 S5: 通知先フォールバックの持ち上げ ----

    /// **旧キーだけの設定ファイルがそのまま動く**（後方互換）。
    ///
    /// `[gateway.discord] default_subtask_webhook` は #157 S5 以前の唯一の書き方。
    /// これが読めなくなると既存の運用が黙って通知を失う。
    #[test]
    fn legacy_discord_webhook_key_is_still_honored() {
        let cfg: AppConfig = toml::from_str(
            r#"
[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok", events = ["started"] }
"#,
        )
        .unwrap();
        let resolved = cfg.default_subtask_webhook().expect("旧キーが読まれるべき");
        assert_eq!(resolved.url, "https://discord.com/api/webhooks/1/legacytok");
        assert_eq!(resolved.events, Some(vec!["started".to_string()]));
    }

    /// 新しい **transport 非依存キー** `[subtask] default_webhook` が読める。
    ///
    /// Discord の設定ブロックが 1 行も無い設定ファイルでも通知先が決まる
    /// （= Discord 機能フラグから独立した）ことがこの持ち上げの要点。
    #[test]
    fn transport_neutral_webhook_key_is_honored_without_any_discord_config() {
        let cfg: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "https://discord.com/api/webhooks/2/neutraltok" }
"#,
        )
        .unwrap();
        let resolved = cfg.default_subtask_webhook().expect("新キーが読まれるべき");
        assert_eq!(
            resolved.url,
            "https://discord.com/api/webhooks/2/neutraltok"
        );
        assert_eq!(resolved.events, None);
    }

    /// 両方書いてあるときは新キーが勝つ（移行期の曖昧さを残さない）。
    #[test]
    fn transport_neutral_webhook_key_wins_over_the_legacy_one() {
        let cfg: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "https://discord.com/api/webhooks/2/neutraltok" }

[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok" }
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.default_subtask_webhook().unwrap().url,
            "https://discord.com/api/webhooks/2/neutraltok"
        );
    }

    /// どちらも無ければ未設定。url が空文字のときも未設定として扱う。
    #[test]
    fn absent_or_empty_webhook_url_resolves_to_none() {
        let empty: AppConfig = toml::from_str("").unwrap();
        assert!(empty.default_subtask_webhook().is_none());

        let blank: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "" }
"#,
        )
        .unwrap();
        assert!(
            blank.default_subtask_webhook().is_none(),
            "url が空なら未設定扱い（`.env` 未設定で ${{VAR}} が空展開される運用）"
        );
    }

    // ---- #207: 新キーが空で旧キーの値を隠すときの警告 ----

    /// 判定の真理値表を網羅で固定する。
    ///
    /// 真になるのは「新キーの節がある × url が空」かつ「旧キーに有効な url がある」の
    /// 1 通りだけ。新キーの節が無ければ旧キーがそのまま使われるので隠していない。
    /// 新キーに url があればそれが使われる（意図どおりの優先）ので警告しない。
    #[test]
    fn masking_predicate_is_true_only_for_empty_new_key_over_a_valid_legacy_one() {
        let cfg = |url: &str| SubtaskWebhookConfig {
            url: url.to_string(),
            events: None,
        };
        let urls = ["", "   ", "https://example.test/hook"];
        for new_url in urls {
            for legacy_url in urls {
                let expected = new_url.trim().is_empty() && !legacy_url.trim().is_empty();
                assert_eq!(
                    legacy_webhook_masked_by_empty_new_key(
                        Some(&cfg(new_url)),
                        Some(&cfg(legacy_url))
                    ),
                    expected,
                    "new={new_url:?} legacy={legacy_url:?}"
                );
            }
            // 旧キーの節が無ければ隠すものが無い。
            assert!(!legacy_webhook_masked_by_empty_new_key(
                Some(&cfg(new_url)),
                None
            ));
            // 新キーの節が無ければ旧キーがそのまま使われる。
            assert!(!legacy_webhook_masked_by_empty_new_key(
                None,
                Some(&cfg(new_url))
            ));
        }
        assert!(!legacy_webhook_masked_by_empty_new_key(None, None));
    }

    /// 踏む経路そのままの設定ファイルで警告条件を満たし、**挙動は変わらない**。
    ///
    /// `${SUBTASK_WEBHOOK_URL}` が `.env` に無いと空文字へ展開されるので、新キーは
    /// `url = ""` と等価になる。
    #[test]
    fn empty_new_key_over_a_valid_legacy_key_warns_without_changing_resolution() {
        let cfg: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "" }

[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok" }
"#,
        )
        .unwrap();
        assert!(
            cfg.legacy_webhook_masked_by_empty_new_key(),
            "新キーが空 + 旧キーに有効な値 → 警告条件を満たす"
        );
        assert!(cfg.warn_if_legacy_webhook_masked());
        assert!(
            cfg.default_subtask_webhook().is_none(),
            "警告を足しても解決順序は変えない（空 url は「無効」のまま）"
        );
    }

    /// 誤検知させない: 警告が「いつも出ている」ものになると誰も読まなくなる。
    #[test]
    fn no_masking_warning_for_the_ordinary_configurations() {
        // 何も書いていない（配布テンプレートの既定）。
        let empty: AppConfig = toml::from_str("").unwrap();
        assert!(!empty.warn_if_legacy_webhook_masked());

        // 旧キーだけ（移行前の既存運用）。
        let legacy_only: AppConfig = toml::from_str(
            r#"
[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok" }
"#,
        )
        .unwrap();
        assert!(!legacy_only.warn_if_legacy_webhook_masked());

        // 新キーだけ（移行後）。
        let new_only: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "https://example.test/hook" }
"#,
        )
        .unwrap();
        assert!(!new_only.warn_if_legacy_webhook_masked());

        // 新キーを空にして意図的に無効化（旧キーも無いので隠していない）。
        let disabled: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "" }
"#,
        )
        .unwrap();
        assert!(!disabled.warn_if_legacy_webhook_masked());
    }

