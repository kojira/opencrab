
/// #691: 応答直前の出力指示（`RESPONSE_ONLY_DIRECTIVE`）が履歴の直後に付くこと、
/// および履歴が空のときは付かないことを固定する。
#[cfg(test)]
mod response_only_directive_tests {
    use super::{build_conversation_string, NO_MESSAGES_MARKER, RESPONSE_ONLY_DIRECTIVE};

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
        // 生成点に最も近い＝出力の末尾に置く。
        assert!(out.trim_end().ends_with(RESPONSE_ONLY_DIRECTIVE), "{out}");
        // 1 回だけ。
        assert_eq!(out.matches(RESPONSE_ONLY_DIRECTIVE).count(), 1);
        // 履歴（発話）は指示より前にある。
        let hist_pos = out.find("こんばんは").unwrap();
        let dir_pos = out.find(RESPONSE_ONLY_DIRECTIVE).unwrap();
        assert!(hist_pos < dir_pos, "{out}");
    }

    #[test]
    fn directive_is_omitted_when_history_is_empty() {
        let conn = opencrab_db::init_memory().unwrap();
        // ログを 1 件も積まない。
        let out = build_conversation_string(&conn, "s1", "a1", 100_000).unwrap();
        assert_eq!(out, NO_MESSAGES_MARKER);
        assert!(!out.contains(RESPONSE_ONLY_DIRECTIVE), "{out}");
    }
}
