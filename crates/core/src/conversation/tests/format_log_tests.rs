
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
    fn issue_975_renders_short_session_call_id_without_provider_id() {
        let provider_id = "toolu_01VWbUdp5dEzpg5SJRTakH5n";
        let tcj = serde_json::json!([{
            "id": provider_id,
            "type": "function",
            "function": { "name": "execute_shell", "arguments": "{\"command\":\"echo\"}" }
        }])
        .to_string();
        let mut log = tool_call_log(&tcj);
        log.metadata_json = Some(
            serde_json::json!({
                "tool_calls_json": tcj,
                "conversation_tool_ids": { provider_id: "t32" }
            })
            .to_string(),
        );

        let out = format_single_log(&log);
        assert!(out.contains("[t32>]"), "短い会話用IDでcallを表示する: {out}");
        assert!(out.contains("execute_shell"), "tool名は保持する: {out}");
        assert!(
            !out.contains(provider_id),
            "長いprovider call IDをモデルへ露出しない: {out}"
        );
    }

    #[test]
    fn issue_975_running_is_not_rendered_as_completed_result() {
        let row = SessionLogRow {
            id: Some(2),
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_result".to_string(),
            content: r#"{"status":"spawned","subtask_id":"internal-uuid"}"#.to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "conversation_tool_id": "t32",
                    "lifecycle_status": "running"
                })
                .to_string(),
            ),
            created_at: None,
        };

        let out = format_single_log(&row);
        assert!(
            out.contains("[<t32] status:running"),
            "開始状態は同じcall IDのrunningとして表示する: {out}"
        );
        assert!(!out.contains("spawned"), "内部状態名を完了結果に見せない: {out}");
        assert!(!out.contains("internal-uuid"), "内部実行IDをモデルへ露出しない: {out}");
        assert!(!out.contains("status:completed"), "runningをcompletedにしない: {out}");
    }

    #[test]
    fn issue_975_completed_result_keeps_same_short_call_id_and_body() {
        let row = SessionLogRow {
            id: Some(3),
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_result".to_string(),
            content: "actual stdout".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "conversation_tool_id": "t32",
                    "lifecycle_status": "completed",
                    "exit_code": 0,
                    "result_omitted": false
                })
                .to_string(),
            ),
            created_at: None,
        };

        let out = format_single_log(&row);
        assert!(out.contains("[<t32] status:completed"), "完了状態: {out}");
        assert!(out.contains("actual stdout"), "現在turnでは実本文を保持する: {out}");
    }

    #[test]
    fn issue_975_reverse_completion_order_does_not_mix_short_call_ids() {
        let completed = |short_id: &str, body: &str| SessionLogRow {
            id: Some(3),
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_result".to_string(),
            content: body.to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "conversation_tool_id": short_id,
                    "lifecycle_status": "completed",
                    "exit_code": 0,
                    "result_omitted": false
                })
                .to_string(),
            ),
            created_at: None,
        };

        // t33が先、t32が後に完了する。
        let t33 = format_single_log(&completed("t33", "second-call-result"));
        let t32 = format_single_log(&completed("t32", "first-call-result"));
        assert!(t33.contains("[<t33] status:completed"), "{t33}");
        assert!(t33.contains("second-call-result"), "{t33}");
        assert!(!t33.contains("t32"), "逆順完了でcall IDを混線しない: {t33}");
        assert!(t32.contains("[<t32] status:completed"), "{t32}");
        assert!(t32.contains("first-call-result"), "{t32}");
        assert!(!t32.contains("t33"), "逆順完了でcall IDを混線しない: {t32}");
    }

    #[test]
    fn issue_975_later_turn_omits_body_but_keeps_auditable_reference() {
        let row = SessionLogRow {
            id: Some(3),
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_result".to_string(),
            content: "must not be reinjected".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "conversation_tool_id": "t32",
                    "lifecycle_status": "completed",
                    "exit_code": 0,
                    "result_omitted": true,
                    "result_path": "tmp/session/t32.txt",
                    "result_bytes": 382004,
                    "result_lines": 2267
                })
                .to_string(),
            ),
            created_at: None,
        };

        let out = format_single_log(&row);
        assert!(out.contains("[<t32] status:completed"), "完了相関を保持する: {out}");
        assert!(out.contains("result_omitted:true"), "縮退を明示する: {out}");
        assert!(out.contains("tmp/session/t32.txt"), "保存先を保持する: {out}");
        assert!(out.contains("382004"), "byte数を保持する: {out}");
        assert!(out.contains("2267"), "行数を保持する: {out}");
        assert!(
            !out.contains("must not be reinjected"),
            "後続turnへ巨大結果本文を再注入しない: {out}"
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
        let tcj = serde_json::json!([{
            "id": "tc-1",
            "type": "function",
            "function": { "name": "search", "arguments": "{\"q\":\"rust\"}" }
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
            !out.contains(r#"{"q":"rust"}"#),
            "完了済み arguments は全文を残さない: {out}"
        );
        let unresolved =
            format_single_log_with_echo(&log, Some(&std::collections::HashSet::new()), None);
        assert!(
            unresolved.contains(r#"{"q":"rust"}"#),
            "未決着 call は全文: {unresolved}"
        );
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

    #[test]
    fn cancelled_batch_uses_short_conversation_ids_without_execution_id() {
        let log = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "s1".to_string(),
            log_type: "tool_cancelled".to_string(),
            content: "task was cancelled".to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "tool_call_id": "t4",
                    "conversation_tool_ids": ["t4", "t5"],
                    "lifecycle_status": "cancelled"
                })
                .to_string(),
            ),
            created_at: None,
        };
        let rendered = format_single_log(&log);
        assert!(rendered.contains("[<t4] status:cancelled"));
        assert!(rendered.contains("[<t5] status:cancelled"));
        assert!(!rendered.contains("legacy_unknown"));
    }
}
