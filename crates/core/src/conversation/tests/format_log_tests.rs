
#[cfg(test)]
mod format_log_tests {
    use super::{format_single_log, format_single_log_with_echo};
    use opencrab_db::queries::SessionLogRow;

    fn tool_call_log(tool_calls_json: &str) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_call".to_string(),
            content: String::new(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({ "tool_calls_json": tool_calls_json }).to_string(),
            ),
            created_at: None,
        }
    }

    #[test]
    fn renders_canonical_tool_call_shape() {
        // 正準形状: {id, type, function:{name, arguments:"<json-string>"}}
        let tcj = serde_json::json!([{
            "id": "tc-1",
            "type": "function",
            "function": { "name": "search", "arguments": "{\"q\":\"rust\"}" }
        }])
        .to_string();
        let out = format_single_log(&tool_call_log(&tcj));
        assert!(out.contains("search"), "tool name must render: {out}");
        assert!(out.contains("tc-1"), "tool id must render: {out}");
        assert!(
            out.contains(r#"{"q":"rust"}"#),
            "arguments must render: {out}"
        );
    }

    #[test]
    fn renders_legacy_flat_tool_call_shape() {
        // 旧形状（既存DB行の後方互換）: {id, name, arguments:<object>}
        let tcj = serde_json::json!([{
            "id": "tc-9",
            "name": "old_tool",
            "arguments": { "a": 1 }
        }])
        .to_string();
        let out = format_single_log(&tool_call_log(&tcj));
        assert!(
            out.contains("old_tool"),
            "legacy tool name must render: {out}"
        );
        assert!(out.contains("tc-9"), "legacy tool id must render: {out}");
    }

    #[test]
    fn completed_tool_call_arguments_become_ref_digest_bytes() {
        // 1,000 トークンを超える引数だけが参照に畳まれる。
        let big_args = serde_json::json!({ "q": "rust ".repeat(3000) }).to_string();
        let tcj = serde_json::json!([{
            "id": "tc-1",
            "type": "function",
            "function": { "name": "search", "arguments": big_args }
        }])
        .to_string();
        let mut log = tool_call_log(&tcj);
        log.id = Some(42);
        let mut done = std::collections::HashSet::new();
        done.insert("tc-1".into());
        let out = format_single_log_with_echo(&log, Some(&done), None);
        assert!(out.contains("search"), "{out}");
        // 完了済み call は log 参照だけ（digest/bytes はモデルに不要なので出さない・row295b）。
        assert!(out.contains("→log:42"), "{out}");
        assert!(!out.contains("digest"), "digest は出さない: {out}");
        assert!(!out.contains("bytes"), "bytes は出さない: {out}");
        assert!(
            !out.contains("rust rust"),
            "大きい完了済み arguments は全文を残さない: {out}"
        );
        let unresolved =
            format_single_log_with_echo(&log, Some(&std::collections::HashSet::new()), None);
        assert!(
            unresolved.contains("rust rust"),
            "未決着 call は全文: {unresolved}"
        );
    }

    /// 小さい引数（1,000 トークン以下）は完了後も**中身のまま次のターンへ残す**。
    ///
    /// 本番（2026-10-03 水平思考クイズ第4問）: owner 起点のターンで ws_write した約 650 字の答えが、
    /// 次のターンでは `ws_write(→log:N)` だけになった。ws_read は owner 専用なので、他の人が起点の
    /// ターンでは答えを読み直せず、判定を保留した。
    #[test]
    fn completed_small_tool_call_arguments_stay_verbatim() {
        let args = serde_json::json!({
            "path": "umigame/q4_answer.md",
            "content": "## 真相\n息子は臓器提供のドナーになり、受け取った人から毎年手紙が届く。"
        })
        .to_string();
        let tcj = serde_json::json!([{
            "id": "tc-1",
            "type": "function",
            "function": { "name": "ws_write", "arguments": args }
        }])
        .to_string();
        let mut log = tool_call_log(&tcj);
        log.id = Some(42);
        let mut done = std::collections::HashSet::new();
        done.insert("tc-1".into());
        let out = format_single_log_with_echo(&log, Some(&done), None);
        assert!(out.contains("毎年手紙が届く"), "小さい引数の中身が消えた: {out}");
        assert!(!out.contains("→log:42"), "小さい引数を参照に畳んだ: {out}");
    }

    /// [#323] 1 つのセッションに複数の相手の発言が混ざっても、**誰の発言かが分かる**。
    ///
    /// Nostr の session を agent 単位（`nostr-{agent_id}`）へ寄せたことで、以前は
    /// 相手ごとに分かれていた会話が 1 本に集まる。会話文字列は `[{speaker_id}]:` 形式で
    /// 出るので、発言者は session ではなく行の `speaker_id` が区別する（Nostr の受信転記は
    /// `speaker_id` に相手の pubkey を入れる）。**新しい概念を足す必要は無い**ことの固定。
    #[test]
    fn different_speakers_in_one_session_stay_distinguishable() {
        let speech = |speaker: &str, text: &str| SessionLogRow {
            id: None,
            agent_id: speaker.to_string(),
            session_id: "nostr-agent-1".to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };

        let alice = format_single_log(&speech("pubkey-alice", "こんばんは"));
        let bob = format_single_log(&speech("pubkey-bob", "こんばんは"));
        let agent = format_single_log(&speech("agent-1", "こんばんは"));

        assert!(alice.starts_with("[pubkey-alice]"), "{alice}");
        assert!(bob.starts_with("[pubkey-bob]"), "{bob}");
        assert!(agent.starts_with("[agent-1]"), "{agent}");
        // 本文が同じでも行としては別物（発言者が潰れていない）。
        assert_ne!(alice, bob);
        assert_ne!(alice, agent);
    }
}
