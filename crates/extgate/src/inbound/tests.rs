#[cfg(test)]
mod cases {
    mod tests {
        use super::super::super::nostr_profile::*;

        #[test]
        fn parse_v1_reply_to_reads_target_and_ignores_null() {
            let with = format!(
                "[NOSTRGATE/V1 {{\"event_id\":\"{}\",\"kind\":1,\"reply_to\":\"{}\"}}]\nhi\n[Nostr kind:1 リプライ]",
                "aa".repeat(32),
                "bb".repeat(32),
            );
            assert_eq!(
                parse_v1_reply_to(&with).as_deref(),
                Some("bb".repeat(32).as_str())
            );
            // null / 欠落は None。
            let without = "[NOSTRGATE/V1 {\"event_id\":\"x\",\"kind\":1,\"reply_to\":null}]\nhi";
            assert_eq!(parse_v1_reply_to(without), None);
            assert_eq!(parse_v1_reply_to("[NOSTRGATE/V1 {\"kind\":1}]\nhi"), None);
        }

        #[test]
        fn renderer_body_strips_v1_line() {
            let text =
                "[NOSTRGATE/V1 {\"kind\":1}]\nhello\n[Nostr kind:1 リプライ from=aa target=note1x]";
            assert_eq!(
                nostr_renderer_body(text),
                "hello\n[Nostr kind:1 リプライ from=aa target=note1x]"
            );
        }

        #[test]
        fn renderer_body_strips_bundle_members_line() {
            let text = "[NOSTRGATE/V1 {\"kind\":1}]\n[NOSTRBUNDLE/V1 [\"o1\"]]\nhello";
            assert_eq!(nostr_renderer_body(text), "hello");
        }

        #[test]
        fn prompt_suffix_omits_raw_identifiers() {
            let pk = "aa".repeat(32);
            let text = format!(
                "[NOSTRGATE/V1 {{\"kind\":1,\"event_id\":\"{}\"}}]\nhello\n[Nostr kind:1 リプライ]",
                "bb".repeat(32)
            );
            let suffix = nostr_prompt_suffix(&pk, &text);
            // nostr_reply 露出撤去（返信は say 一本 / #840）: プロンプトに nostr_reply を出さない。
            assert!(!suffix.contains("nostr_reply"));
            // §9A / row296: 生 ID（対象ノート note1・pubkey hex）をプロンプトから排除する。
            assert!(!suffix.contains("note1"));
            assert!(!suffix.contains("pubkey="));
            assert!(!suffix.contains(&pk));
            assert!(!suffix.contains("対象ノート"));
            // §9A/DI-16: 普通の投稿は本文をそのまま書く（standalone）、返信は reply(e番号) 操作。
            assert!(suffix.contains("本文をそのまま書いて"));
            assert!(suffix.contains("reply(e番号"));
            assert!(suffix.contains("kind:1"));
            assert!(suffix.contains("NO_REPLY"));
        }
    }
}
