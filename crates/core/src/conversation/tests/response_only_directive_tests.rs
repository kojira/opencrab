
/// #691: 会話履歴だけが構造タグで囲まれること、および履歴が空のときは
/// タグを付けないことを固定する。
#[cfg(test)]
mod response_only_directive_tests {
    use super::{
        build_conversation_string, CONVERSATION_HISTORY_END, CONVERSATION_HISTORY_START,
        NO_MESSAGES_MARKER,
    };

    fn seed_speech(conn: &rusqlite::Connection, speaker: &str, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "a1".to_string(),
                session_id: "s1".to_string(),
                log_type: "speech".to_string(),
                content: content.to_string(),
                speaker_id: Some(speaker.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn directive_is_appended_after_history() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_speech(&conn, "owner", "こんばんは");
        let out = build_conversation_string(&conn, "s1", "a1", 100_000).unwrap();
        assert!(out.trim_end().ends_with(CONVERSATION_HISTORY_END), "{out}");
        assert_eq!(out.matches(CONVERSATION_HISTORY_START).count(), 1);
        assert_eq!(out.matches(CONVERSATION_HISTORY_END).count(), 1);
        let start = out.find(CONVERSATION_HISTORY_START).unwrap();
        let speech = out.find("こんばんは").unwrap();
        let end = out.find(CONVERSATION_HISTORY_END).unwrap();
        assert!(start < speech && speech < end, "{out}");
        assert!(!out.contains("ここから先はあなた自身の本文のみを書く"));
    }

    #[test]
    fn directive_is_omitted_when_history_is_empty() {
        let conn = opencrab_db::init_memory().unwrap();
        let out = build_conversation_string(&conn, "s1", "a1", 100_000).unwrap();
        assert_eq!(out, NO_MESSAGES_MARKER);
        assert!(!out.contains(CONVERSATION_HISTORY_START), "{out}");
        assert!(!out.contains(CONVERSATION_HISTORY_END), "{out}");
    }
}
