    fn insert_row(
        conn: &rusqlite::Connection,
        scope: &str,
        agent_id: &str,
        tool_name: &str,
        kind: &str,
        url: &str,
        enabled: bool,
    ) {
        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: scope.to_string(),
            agent_id: agent_id.to_string(),
            tool_name: tool_name.to_string(),
            kind: kind.to_string(),
            url: url.to_string(),
            events_json: None,
            enabled,
            name: None,
            created_by: Some("owner".to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };
        opencrab_db::queries::upsert_agent_webhook_config(conn, &row).unwrap();
    }

    fn use_source(r: &WebhookResolution) -> WebhookSource {
        match r {
            WebhookResolution::Use { source, .. } => *source,
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn test_webhook_resolution_explicit_beats_db() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": VALID_URL } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::Explicit);
    }

    #[test]
    fn test_webhook_resolution_tool_beats_agent_beats_global() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "global", "*", "", "subtask", VALID_URL, true);
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        insert_row(
            &conn,
            "tool",
            "a1",
            "spawn_subtask",
            "subtask",
            VALID_URL,
            true,
        );
        let args = json!({});
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::ToolDefault);

        // remove tool -> agent wins
        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "global", "*", "", "subtask", VALID_URL, true);
        insert_row(&conn2, "agent", "a1", "", "subtask", VALID_URL, true);
        let r2 = resolve_subtask_webhook(&conn2, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r2), WebhookSource::AgentDefault);

        // only global
        let conn3 = opencrab_db::init_memory().unwrap();
        insert_row(&conn3, "global", "*", "", "subtask", VALID_URL, true);
        let r3 = resolve_subtask_webhook(&conn3, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r3), WebhookSource::GlobalDefault);
    }

    #[test]
    fn test_webhook_resolution_db_beats_env_config() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: "https://discord.com/api/webhooks/9/envtok".to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_env_only_when_no_db_row() {
        let conn = opencrab_db::init_memory().unwrap();
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        assert_eq!(use_source(&r), WebhookSource::EnvConfig);
    }

    #[test]
    fn test_webhook_resolution_none() {
        let conn = opencrab_db::init_memory().unwrap();
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert!(matches!(r, WebhookResolution::None));
    }

    #[test]
    fn test_webhook_resolution_invalid_explicit() {
        let conn = opencrab_db::init_memory().unwrap();
        let args = json!({ "webhook": { "url": "http://evil.com/x" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
            }
            _ => panic!("expected Error"),
        }
    }

    // ---- empty / whitespace explicit url falls back to default (not an error) ----

    #[test]
    fn test_webhook_resolution_empty_explicit_url_falls_back_to_db_default() {
        // 明示 webhook の url が空文字なら「指定なし」扱いとし、DB の agent デフォルトへ
        // フォールバックする（Error にして配送をブロックしない）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_whitespace_explicit_url_falls_back_to_db_default() {
        // 空白のみの url も「指定なし」扱い。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "   " } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_falls_back_to_env_config() {
        // DB 行が無くても、空 url は env/config デフォルトへフォールバックする。
        let conn = opencrab_db::init_memory().unwrap();
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let args = json!({ "webhook": { "url": "" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, Some(&env));
        assert_eq!(use_source(&r), WebhookSource::EnvConfig);
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_with_no_default_is_none() {
        // 空 url + デフォルト無し → None（Error ではない）。
        let conn = opencrab_db::init_memory().unwrap();
        let args = json!({ "webhook": { "url": "   " } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        assert!(matches!(r, WebhookResolution::None));
    }

    #[test]
    fn test_webhook_resolution_empty_explicit_url_keeps_events_ignored_on_fallback() {
        // 空 url のとき explicit events は使われず、フォールバック先（DB）の設定が勝つ。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "", "events": ["completed"] } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Use { config, source } => {
                assert_eq!(source, WebhookSource::AgentDefault);
                assert_eq!(config.url, VALID_URL);
                // DB 行は events_json=None なので全イベント送信。
                assert_eq!(config.events, None);
            }
            _ => panic!("expected Use from DB default"),
        }
    }

    #[test]
    fn test_webhook_resolution_nonempty_invalid_explicit_still_errors_over_default() {
        // 非空の不正 url はデフォルトがあっても fall through せず Error（strict 維持）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let args = json!({ "webhook": { "url": "http://evil.com/x/secrettok" } });
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &args, None);
        match r {
            WebhookResolution::Error {
                code,
                message,
                source,
            } => {
                assert_eq!(code, "invalid_webhook_url");
                assert_eq!(source, WebhookSource::Explicit);
                // 診断メッセージに raw url/token は漏れない。
                assert!(!message.contains("secrettok"), "token leaked: {message}");
            }
            _ => panic!("expected Error, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_invalid_db_default_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        // tool default invalid, agent default valid -> must NOT fall through.
        insert_row(
            &conn,
            "tool",
            "a1",
            "spawn_subtask",
            "subtask",
            "http://bad",
            true,
        );
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_default_webhook");
                assert_eq!(source, WebhookSource::ToolDefault);
            }
            _ => panic!("expected Error, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_disabled_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        // tool disabled, agent valid -> Disabled, no fallthrough.
        insert_row(
            &conn,
            "tool",
            "a1",
            "spawn_subtask",
            "subtask",
            VALID_URL,
            false,
        );
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let env = WebhookConfig {
            url: VALID_URL.to_string(),
            events: None,
        };
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), Some(&env));
        match r {
            WebhookResolution::Disabled { source } => {
                assert_eq!(source, WebhookSource::ToolDefault);
            }
            _ => panic!("expected Disabled, got fallthrough"),
        }
    }

    #[test]
    fn test_webhook_resolution_lifecycle_alias() {
        let conn = opencrab_db::init_memory().unwrap();
        // only a 'lifecycle' row at agent scope -> resolves like subtask.
        insert_row(&conn, "agent", "a1", "", "lifecycle", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);

        // lifecycle at tool scope still beats agent-scope subtask row.
        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "agent", "a1", "", "subtask", VALID_URL, true);
        insert_row(
            &conn2,
            "tool",
            "a1",
            "spawn_subtask",
            "lifecycle",
            VALID_URL,
            true,
        );
        let r2 = resolve_subtask_webhook(&conn2, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r2), WebhookSource::ToolDefault);
    }

    // ---- secret redaction ----

    #[test]
    fn test_redact_secrets_scrubs_known_patterns() {
        let input =
            "key sk-ABCDEFGHIJKLMNOP and ghp_0123456789abcdefghij and AKIAABCDEFGHIJKLMNOP \
                     Authorization: Bearer myreallylongtoken123456 \
                     API_KEY=supersecretvalue \
                     hook https://discord.com/api/webhooks/123/abcdefSECRETtoken \
                     hex 0123456789abcdef0123456789abcdef0123";
        let out = redact_secrets(input);
        assert!(!out.contains("sk-ABCDEFGHIJKLMNOP"), "sk leaked: {out}");
        assert!(
            !out.contains("ghp_0123456789abcdefghij"),
            "ghp leaked: {out}"
        );
        assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"), "akia leaked: {out}");
        assert!(
            !out.contains("myreallylongtoken123456"),
            "bearer leaked: {out}"
        );
        assert!(!out.contains("supersecretvalue"), "kv leaked: {out}");
        assert!(
            !out.contains("abcdefSECRETtoken"),
            "webhook token leaked: {out}"
        );
        assert!(out.contains("[REDACTED]"));
        // benign words preserved
        assert!(out.contains("key"));
        assert!(out.contains("Authorization:"));
    }

    #[test]
    fn test_redact_secrets_kv_value_in_next_token() {
        let out = redact_secrets("\"token\": \"abcdefghijklmnopqrstuvwx\"");
        assert!(
            !out.contains("abcdefghijklmnopqrstuvwx"),
            "value leaked: {out}"
        );
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn test_redact_secrets_idempotent_and_keeps_plain_text() {
        let plain = "hello world exit=0 done";
        assert_eq!(redact_secrets(plain), plain);
        let once = redact_secrets("API_KEY=supersecretvalue");
        let twice = redact_secrets(&once);
        assert_eq!(once, twice);
    }

    // ---- activity-family resolution ----

    #[test]
    fn test_resolve_activity_tool_beats_agent_beats_global() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "global", "*", "", "activity", VALID_URL, true);
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        insert_row(
            &conn,
            "tool",
            "a1",
            "execute_shell",
            "activity",
            VALID_URL,
            true,
        );
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert_eq!(use_source(&r), WebhookSource::ToolDefault);

        let conn2 = opencrab_db::init_memory().unwrap();
        insert_row(&conn2, "global", "*", "", "activity", VALID_URL, true);
        insert_row(&conn2, "agent", "a1", "", "activity", VALID_URL, true);
        let r2 = resolve_activity_webhook(&conn2, "a1", "execute_shell");
        assert_eq!(use_source(&r2), WebhookSource::AgentDefault);

        let conn3 = opencrab_db::init_memory().unwrap();
        insert_row(&conn3, "global", "*", "", "activity", VALID_URL, true);
        let r3 = resolve_activity_webhook(&conn3, "a1", "execute_shell");
        assert_eq!(use_source(&r3), WebhookSource::GlobalDefault);
    }

    #[test]
    fn test_resolve_activity_ignores_subtask_kind_and_has_no_env() {
        let conn = opencrab_db::init_memory().unwrap();
        // only a subtask-kind agent row exists -> activity resolution must NOT use it.
        insert_row(&conn, "agent", "a1", "", "subtask", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert!(
            matches!(r, WebhookResolution::None),
            "subtask kind must not serve activity"
        );
    }

    #[test]
    fn test_resolve_activity_disabled_no_fallthrough() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(
            &conn,
            "tool",
            "a1",
            "execute_shell",
            "activity",
            VALID_URL,
            false,
        );
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        assert!(matches!(
            r,
            WebhookResolution::Disabled {
                source: WebhookSource::ToolDefault
            }
        ));
    }

    #[test]
    fn test_resolve_activity_invalid_db_default_errors() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", "http://bad", true);
        let r = resolve_activity_webhook(&conn, "a1", "execute_shell");
        match r {
            WebhookResolution::Error { code, source, .. } => {
                assert_eq!(code, "invalid_default_webhook");
                assert_eq!(source, WebhookSource::AgentDefault);
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_resolve_activity_also_serves_subtask_lifecycle() {
        // An agent 'activity' default should also be picked up by resolve_subtask_webhook
        // (activity family includes subtask lifecycle).
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    #[test]
    fn test_resolve_subtask_prefers_explicit_subtask_over_activity_same_scope() {
        // L3: 同一 scope に subtask 専用行と汎用 activity 行が両方あるとき、subtask 通知は
        // 明示的な subtask 専用デフォルトへ送る（activity に奪われない）。
        const SUBTASK_URL: &str = "https://discord.com/api/webhooks/111/subtasktoken";
        const ACTIVITY_URL: &str = "https://discord.com/api/webhooks/222/activitytoken";
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", ACTIVITY_URL, true);
        insert_row(&conn, "agent", "a1", "", "subtask", SUBTASK_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        match r {
            WebhookResolution::Use { config, source } => {
                assert_eq!(source, WebhookSource::AgentDefault);
                assert_eq!(
                    config.url, SUBTASK_URL,
                    "subtask-specific default must win over generic activity"
                );
            }
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn test_resolve_subtask_falls_back_to_activity_when_no_subtask_row() {
        // subtask 専用行が無ければ activity 行へフォールバックする（family 包含）。
        let conn = opencrab_db::init_memory().unwrap();
        insert_row(&conn, "agent", "a1", "", "activity", VALID_URL, true);
        let r = resolve_subtask_webhook(&conn, "a1", "spawn_subtask", &json!({}), None);
        assert_eq!(use_source(&r), WebhookSource::AgentDefault);
    }

    // ---- delivery failure recording ----

    #[test]
    fn test_record_webhook_delivery_failure_writes_redacted_log() {
        let conn = opencrab_db::init_memory().unwrap();

        let redacted = redact_webhook_url(VALID_URL);
        record_webhook_delivery_failure(
            &conn,
            "a1",
            "parent-sess",
            "st1",
            "subtask-st1",
            &redacted,
            "http 500",
        );

        let logs =
            opencrab_db::queries::list_session_logs_by_session(&conn, "parent-sess").unwrap();
        let found = logs
            .iter()
            .find(|l| l.content.contains("delivery_failed"))
            .expect("delivery_failed log should exist");
        assert!(found.content.contains("[redacted]"));
        assert!(
            !found.content.contains(SECRET_TOKEN),
            "raw token leaked into log: {}",
            found.content
        );

        // empty parent_session_id -> no-op
        record_webhook_delivery_failure(&conn, "a1", "", "st1", "s", &redacted, "x");
    }

    // ---- Nostr 受信 → Discord 転記先の解決（#252 段階 A） ----

    fn set_relay(conn: &rusqlite::Connection, agent_id: &str, enabled: bool, url: Option<&str>) {
        opencrab_db::queries::upsert_agent_nostr_relay_config(
            conn,
            &opencrab_db::queries::AgentNostrRelayConfigRow {
                agent_id: agent_id.to_string(),
                enabled,
                webhook_url: url.map(|s| s.to_string()),
            },
        )
        .unwrap();
    }

    /// fail-closed: 未設定 / 無効 / URL 欠落・不正 はすべて「転記しない（None）」。
    #[test]
    fn test_resolve_nostr_relay_is_fail_closed() {
        let conn = opencrab_db::init_memory().unwrap();

        // 1. 行が無い → None。
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 2. enabled=false（URL はあっても無効なら転記しない）。
        set_relay(&conn, "a1", false, Some(VALID_URL));
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 3. enabled だが URL が NULL。
        set_relay(&conn, "a1", true, None);
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 4. enabled だが URL が空白のみ。
        set_relay(&conn, "a1", true, Some("   "));
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());

        // 5. enabled だが Discord webhook として不正な URL。
        set_relay(&conn, "a1", true, Some("http://evil.com/x/tok"));
        assert!(resolve_nostr_relay_webhook(&conn, "a1").is_none());
    }

    /// 有効かつ URL が妥当なら、その宛先を全イベント（events=None）で返す。
    #[test]
    fn test_resolve_nostr_relay_returns_target_when_enabled_and_valid() {
        let conn = opencrab_db::init_memory().unwrap();
        set_relay(&conn, "a1", true, Some(VALID_URL));
        let cfg = resolve_nostr_relay_webhook(&conn, "a1").expect("有効なら宛先を返す");
        assert_eq!(cfg.url, VALID_URL);
        assert_eq!(cfg.events, None, "転記は種別で間引かない");

        // 前後空白は trim される。
        set_relay(
            &conn,
            "a2",
            true,
            Some("  https://discord.com/api/webhooks/9/tok9  "),
        );
        let cfg = resolve_nostr_relay_webhook(&conn, "a2").unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/9/tok9");

        // per-agent: 別エージェントは設定を共有しない。
        assert!(resolve_nostr_relay_webhook(&conn, "a3").is_none());
    }
