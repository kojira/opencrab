
/// `[Impressions]` セクションが会話文字列に載ること（#314）。
///
/// **相手が変わればセクションの中身も変わる**（全員分を常に載せない）。相手の
/// 人物像が無い場合はセクション自体が出ず、会話の組み立ては壊れない。
#[cfg(test)]
mod impression_section_injection_tests {
    use super::build_conversation_string;

    const AGENT: &str = "a1";

    fn insert_speech(conn: &rusqlite::Connection, session_id: &str, speaker_id: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: speaker_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: "こんにちは".to_string(),
                speaker_id: Some(speaker_id.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    fn write_impression(conn: &rusqlite::Connection, session_id: &str, target_id: &str) {
        opencrab_db::queries::upsert_impression(
            conn,
            &opencrab_db::queries::ImpressionRow {
                id: format!("imp-{target_id}"),
                agent_id: AGENT.to_string(),
                session_id: session_id.to_string(),
                target_id: target_id.to_string(),
                target_name: format!("name-{target_id}"),
                personality: format!("personality-{target_id}"),
                communication_style: String::new(),
                recent_behavior: String::new(),
                agreement: "中立".to_string(),
                notes: String::new(),
                last_updated_turn: 0,
            },
        )
        .unwrap();
    }

    /// 別経路（別セッション）で書いた人物像が、いま話しているセッションのプロンプトに載る。
    #[test]
    fn injects_impression_of_the_current_speaker_across_sessions() {
        let conn = opencrab_db::init_memory().unwrap();
        write_impression(&conn, "discord-sess", "u1");
        insert_speech(&conn, "nostr-sess", "u1");

        let out = build_conversation_string(&conn, "nostr-sess", AGENT, 100_000).unwrap();
        assert_eq!(out.matches("[Impressions]").count(), 1);
        assert!(out.contains("personality-u1"), "{out}");
    }

    /// 話していない相手の人物像は載らない。
    #[test]
    fn omits_impressions_of_people_not_speaking() {
        let conn = opencrab_db::init_memory().unwrap();
        write_impression(&conn, "s1", "u1");
        write_impression(&conn, "s1", "u2");
        insert_speech(&conn, "s1", "u1");

        let out = build_conversation_string(&conn, "s1", AGENT, 100_000).unwrap();
        assert!(out.contains("personality-u1"), "{out}");
        assert!(!out.contains("personality-u2"), "{out}");
    }

    /// 相手の人物像が無くてもセクションが出ないだけで、会話は普通に組み立つ。
    #[test]
    fn no_impression_means_no_section() {
        let conn = opencrab_db::init_memory().unwrap();
        insert_speech(&conn, "s1", "u1");

        let out = build_conversation_string(&conn, "s1", AGENT, 100_000).unwrap();
        assert!(!out.contains("[Impressions]"), "{out}");
        assert!(out.contains("こんにちは"), "{out}");
    }
}
