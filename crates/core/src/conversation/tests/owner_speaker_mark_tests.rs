/// D-1056: 受信記録時に owner と判定された発言は、会話履歴の話者表示に `|owner` が付く。
#[cfg(test)]
mod owner_speaker_mark_tests {
    use super::*;
    use opencrab_db::queries::SessionLogRow;

    fn speech(agent: &str, speaker: &str, meta: serde_json::Value) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "speech".into(),
            content: "hi".into(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        }
    }

    #[test]
    fn owner_marked_speech_renders_owner_suffix_others_do_not() {
        let logs = vec![
            speech("me", "owner-key", serde_json::json!({"user_name": "kojira", OWNER_SPEAKER_METADATA: true})),
            speech("me", "owner-key", serde_json::json!({OWNER_SPEAKER_METADATA: true})),
            speech("me", "guest-key", serde_json::json!({"user_name": "guest"})),
            speech("me", "old-owner", serde_json::json!({"user_name": "kojira"})),
            speech("me", "me", serde_json::json!({OWNER_SPEAKER_METADATA: true})),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        let render = |i: usize| format_single_log_with_echo(&logs[i], None, Some(&refs));
        assert!(render(0).starts_with("[u1|kojira|owner]:"), "{}", render(0));
        assert!(render(1).starts_with("[u1|owner]:"), "{}", render(1));
        assert!(render(2).starts_with("[u2|guest]:"), "{}", render(2));
        // 印の無い既存ログ（記録時に owner 情報が無い）には付かない。
        assert!(render(3).starts_with("[u3|kojira]:"), "{}", render(3));
        // 自分自身の発言には付けない。
        assert!(!render(4).contains("|owner"), "{}", render(4));
    }
}
