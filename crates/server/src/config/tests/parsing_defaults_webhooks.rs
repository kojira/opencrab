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

    /// Empty transport-neutral webhook settings disable delivery.
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
