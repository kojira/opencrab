    #[test]
    fn test_chunk_text_empty() {
        assert!(chunk_text("", 10).is_empty());
        assert!(chunk_text("abc", 0).is_empty());
    }

    #[test]
    fn test_chunk_text_shorter_than_limit() {
        let chunks = chunk_text("hello", 10);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn test_chunk_text_splits_in_order() {
        let chunks = chunk_text("abcdefg", 3);
        assert_eq!(chunks, vec!["abc", "def", "g"]);
        // reconstruction preserves order/content
        assert_eq!(chunks.concat(), "abcdefg");
    }

    #[test]
    fn test_chunk_text_respects_utf8_boundaries() {
        // multibyte chars must not be split mid-byte
        let chunks = chunk_text("あいうえお", 2);
        assert_eq!(chunks, vec!["あい", "うえ", "お"]);
        assert_eq!(chunks.concat(), "あいうえお");
    }

    #[test]
    fn test_build_relay_webhook_body_has_content() {
        let body = build_relay_webhook_body("hello");
        assert_eq!(body["content"], json!("hello"));
    }

    // ---- プレビュー + 全文添付（#293） ----

    /// 閾値以下は添付せず、本文もそのまま（従来どおり JSON 1 本で飛ぶ）。回帰テスト。
    #[test]
    fn short_text_is_not_attached() {
        for len in [0usize, 1, 100, ATTACHMENT_THRESHOLD_CHARS] {
            let text = "a".repeat(len);
            let m = build_message_with_optional_attachment(&text, "x");
            assert!(!m.has_attachment(), "len={len} が添付になった");
            assert_eq!(m.content, text, "len={len} で本文が改変された");
        }
    }

    /// 閾値を 1 文字でも超えたら「プレビュー + 全文添付」に切り替わる。
    #[test]
    fn long_text_becomes_preview_plus_attachment() {
        let text = "b".repeat(ATTACHMENT_THRESHOLD_CHARS + 1);
        let m = build_message_with_optional_attachment(&text, "unit");
        let att = m.attachment.as_ref().expect("添付されるはず");
        assert_eq!(att.filename, "unit.txt");
        assert_eq!(att.content_type, ATTACHMENT_CONTENT_TYPE);
        assert!(!att.truncated);
        // 全文がそのまま添付になる（ロスなし）。
        assert_eq!(att.data, text.as_bytes());
        assert_eq!(m.delivered_text(), text);
        // プレビューは指定長ちょうど。
        assert_eq!(
            m.content.chars().take_while(|c| *c == 'b').count(),
            ATTACHMENT_PREVIEW_CHARS
        );
        assert!(m.content.contains("full text attached as `unit.txt`"));
        assert!(m
            .content
            .contains(&format!("{} chars", text.chars().count())));
        // 本文は Discord の 1 通上限に収まる。
        assert!(m.content.chars().count() <= 2000);
    }

    /// プレビュー長は既定より**短く**する方向にだけ効く（webhook 設定の max_chars）。
    #[test]
    fn preview_length_can_be_shortened_but_not_extended() {
        let text = "c".repeat(5000);
        let short = build_message_with_attachment_preview(&text, "u", 50);
        assert_eq!(short.content.chars().take_while(|c| *c == 'c').count(), 50);
        // 上へは伸びない（既定で頭打ち）。
        let long = build_message_with_attachment_preview(&text, "u", 100_000);
        assert_eq!(
            long.content.chars().take_while(|c| *c == 'c').count(),
            ATTACHMENT_PREVIEW_CHARS
        );
        // 0 は既定扱い。
        let zero = build_message_with_attachment_preview(&text, "u", 0);
        assert_eq!(zero.content, long.content);
    }

    /// マルチバイトでもプレビューは文字単位で切り、添付は全文のまま。
    #[test]
    fn preview_respects_utf8_boundaries() {
        let text = "あ".repeat(ATTACHMENT_THRESHOLD_CHARS + 10);
        let m = build_message_with_optional_attachment(&text, "u");
        assert_eq!(
            m.content.chars().take_while(|c| *c == 'あ').count(),
            ATTACHMENT_PREVIEW_CHARS
        );
        assert_eq!(m.delivered_text(), text);
    }

    /// サイズ上限超過は**送信前に**切り詰め、省略した旨を本文にも添付にも残す。
    #[test]
    fn oversized_attachment_is_truncated_before_send() {
        let text = "d".repeat(ATTACHMENT_MAX_BYTES + 10_000);
        let m = build_message_with_optional_attachment(&text, "u");
        let att = m.attachment.as_ref().unwrap();
        assert!(att.truncated);
        assert!(
            att.data.len() <= ATTACHMENT_MAX_BYTES,
            "cap を超えた: {}",
            att.data.len()
        );
        assert!(m.delivered_text().contains("[truncated]"));
        assert!(m.content.contains("truncated"));
    }

    /// 切り詰めは UTF-8 境界を壊さない。
    #[test]
    fn truncation_keeps_utf8_valid() {
        // 3 バイト文字で埋めて境界をまたぐようにする。
        let text = "漢".repeat(ATTACHMENT_MAX_BYTES / 3 + 100);
        let m = build_message_with_optional_attachment(&text, "u");
        let att = m.attachment.as_ref().unwrap();
        assert!(att.truncated);
        std::str::from_utf8(&att.data).expect("添付が不正な UTF-8 になった");
    }

    /// ファイル名は静的な語彙だけを通し、秘密や個人情報が混ざりうる文字は潰す。
    #[test]
    fn attachment_filename_is_sanitized() {
        assert_eq!(attachment_filename("subtask-task"), "subtask-task.txt");
        assert_eq!(
            attachment_filename("tool_call_completed-execute_shell"),
            "tool_call_completed-execute_shell.txt"
        );
        // パス区切り・空白・記号は潰れる（ディレクトリ脱出も起きない）。
        assert_eq!(attachment_filename("../../etc/passwd"), "etc-passwd.txt");
        assert_eq!(attachment_filename("a b\tc"), "a-b-c.txt");
        // 非 ASCII だけの slug（名前・本文の断片）は丸ごと落ちて既定名になる。
        assert_eq!(attachment_filename("日本語のみ"), "output.txt");
        // 空・記号のみは既定名へフォールバック。
        assert_eq!(attachment_filename(""), "output.txt");
        assert_eq!(attachment_filename("..."), "output.txt");
        // 長すぎる名前は切り詰める。
        let long = attachment_filename(&"z".repeat(500));
        assert!(long.len() <= 52, "filename too long: {long}");
        assert!(long.ends_with(".txt"));
    }

    /// 添付は本文と**同じ文字列**から作られる ＝ 上流のマスクが添付にも効く。
    #[test]
    fn attachment_is_built_from_the_same_masked_string() {
        // 上流（crates/nostr の mask_secrets）を通った後の形を模す。
        let masked = "secret_key = \"<redacted>\" nsec1<redacted>\n".repeat(200);
        let m = build_message_with_optional_attachment(&masked, "u");
        let full = m.delivered_text();
        assert!(full.contains("nsec1<redacted>"));
        assert!(!full.contains("nsec1qqqqqqq"));
        // 添付本体はプレビュー元の文字列そのもの。
        assert!(full.starts_with(&m.content.chars().take(40).collect::<String>()));
    }

    #[test]
    fn webhook_body_shape() {
        assert_eq!(build_webhook_body("hi", false), json!({ "content": "hi" }));
        assert_eq!(
            build_webhook_body("hi", true),
            json!({ "content": "hi", "allowed_mentions": { "parse": [] } })
        );
    }

    #[test]
    fn test_build_relay_webhook_body_suppresses_all_mentions() {
        // allowed_mentions.parse は必ず空配列で乗る（mention 暴発抑止の固定）。
        let body = build_relay_webhook_body("plain text");
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        // 空配列であること（省略でも非空でもない）を厳密に確認。
        let parse = body["allowed_mentions"]["parse"]
            .as_array()
            .expect("parse must be an array");
        assert!(parse.is_empty(), "parse must be empty to suppress mentions");
    }

    #[test]
    fn test_build_relay_webhook_body_suppresses_everyone_input() {
        // 第三者が @everyone 等を含むリプライを送っても、body は content をそのまま
        // 載せつつ allowed_mentions.parse: [] で全解決を止める。
        let hostile = "@everyone @here <@123> <@&456> pwn";
        let body = build_relay_webhook_body(hostile);
        assert_eq!(body["content"], json!(hostile));
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn test_webhook_config_from_args() {
        let cfg = WebhookConfig::from_args(&json!({
            "webhook": { "url": "https://discord.com/api/webhooks/x", "events": ["started", "completed"] }
        }))
        .unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/x");
        assert_eq!(
            cfg.events,
            Some(vec!["started".to_string(), "completed".to_string()])
        );
    }

    #[test]
    fn test_webhook_config_from_args_no_events() {
        let cfg = WebhookConfig::from_args(&json!({
            "webhook": { "url": "https://x" }
        }))
        .unwrap();
        assert_eq!(cfg.events, None);
    }

    #[test]
    fn test_webhook_config_from_args_missing_or_empty() {
        assert!(WebhookConfig::from_args(&json!({})).is_none());
        assert!(WebhookConfig::from_args(&json!({ "webhook": {} })).is_none());
        assert!(WebhookConfig::from_args(&json!({ "webhook": { "url": "" } })).is_none());
        // 空白のみの url も「指定なし」として None（フォールバック可能）。
        assert!(WebhookConfig::from_args(&json!({ "webhook": { "url": "   " } })).is_none());
    }

    #[test]
    fn test_webhook_config_from_parts_missing_or_empty() {
        assert!(WebhookConfig::from_parts("".to_string(), None).is_none());
        assert!(WebhookConfig::from_parts("   ".to_string(), None).is_none());

        let cfg = WebhookConfig::from_parts(
            "https://discord.com/api/webhooks/x".to_string(),
            Some(vec!["started".to_string()]),
        )
        .unwrap();
        assert_eq!(cfg.url, "https://discord.com/api/webhooks/x");
        assert_eq!(cfg.events, Some(vec!["started".to_string()]));
    }

    #[test]
    fn test_webhook_config_wants() {
        let all = WebhookConfig {
            url: "u".to_string(),
            events: None,
        };
        assert!(all.wants("started"));
        assert!(all.wants("progress"));
        assert!(all.wants("aborted"));

        let filtered = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["completed".to_string()]),
        };
        assert!(filtered.wants("completed"));
        assert!(!filtered.wants("started"));
        assert!(!filtered.wants("progress"));

        let lifecycle = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["started".to_string(), "completed".to_string()]),
        };
        assert!(lifecycle.wants("progress"));

        let fully_qualified = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["subtask.started".to_string()]),
        };
        assert!(fully_qualified.wants("started"));

        // Regression: depth0 sink emits `tool_call_*`; the stored allow-list uses the
        // canonical status vocabulary. Both sides must normalize to the same token so
        // activity events are not silently dropped before HTTP delivery.
        let activity_legacy = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec![
                "started".to_string(),
                "progress".to_string(),
                "completed".to_string(),
                "failed".to_string(),
                "timed_out".to_string(),
            ]),
        };
        assert!(activity_legacy.wants("tool_call_started"));
        assert!(activity_legacy.wants("tool_call_completed"));
        assert!(activity_legacy.wants("tool_call_failed"));
        // `rejected` is a tool-only status absent from this legacy list, so it stays
        // filtered here; an all-events (None) config delivers it.
        assert!(!activity_legacy.wants("tool_call_rejected"));

        let activity_explicit = WebhookConfig {
            url: "u".to_string(),
            events: Some(vec!["rejected".to_string(), "tool_call_failed".to_string()]),
        };
        assert!(activity_explicit.wants("tool_call_rejected"));
        assert!(activity_explicit.wants("tool_call_failed"));
        assert!(!activity_explicit.wants("tool_call_started"));
    }

    // ---- webhook URL validation ----

    const VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcdefSECRETtoken";
    const SECRET_TOKEN: &str = "abcdefSECRETtoken";

    #[test]
    fn test_validate_webhook_url_valid() {
        assert!(validate_webhook_url(VALID_URL).is_ok());
        assert!(validate_webhook_url("https://canary.discord.com/api/webhooks/1/tok").is_ok());
        assert!(validate_webhook_url("https://discordapp.com/api/webhooks/1/tok").is_ok());
        assert!(validate_webhook_url("https://ptb.discord.com/api/webhooks/1/tok").is_ok());
    }

    #[test]
    fn test_validate_webhook_url_invalid() {
        assert!(validate_webhook_url("").is_err());
        assert!(validate_webhook_url("   ").is_err());
        assert!(validate_webhook_url("http://discord.com/api/webhooks/1/tok").is_err());
        assert!(validate_webhook_url("https://evil.com/api/webhooks/1/tok").is_err());
        // missing token segment
        assert!(validate_webhook_url("https://discord.com/api/webhooks/123").is_err());
        // wrong path
        assert!(validate_webhook_url("https://discord.com/channels/1/2").is_err());
        // no path
        assert!(validate_webhook_url("https://discord.com").is_err());
        // reason must not leak the raw url
        let reason = validate_webhook_url("https://evil.com/api/webhooks/1/secrettok").unwrap_err();
        assert!(!reason.contains("secrettok"));
    }

    // ---- redaction ----

    #[test]
    fn test_redact_webhook_url_hides_token() {
        let redacted = redact_webhook_url(VALID_URL);
        assert!(!redacted.contains(SECRET_TOKEN), "token leaked: {redacted}");
        assert!(redacted.contains("[redacted]"));
        assert!(redacted.contains("123456789"));
    }

    // ---- resolution ----

