/// D-1085 §13.6: 既定プロフィールと軽量ターンは同じ組立の 1 実装。履歴の取り方だけを
/// [`ConversationWindow`] で選ぶ。ここは 2 つの入口が同じ関数へ落ちることを示す。
///
/// - `window: None` は既定の履歴（前置・snapshot+差分・圧縮）そのもの。
/// - 窓が会話全体より広ければ、窓の履歴は既定の履歴の履歴本文と同じ行を並べる
///   （前置の有無だけが違い、それは §13.1 の仕様）。
#[cfg(test)]
mod window_builder_tests {
    use crate::conversation::{
        build_conversation_string_with_waters, build_conversation_string_with_window,
        ConversationWindow, RecentWindow, CONVERSATION_HISTORY_END, CONVERSATION_HISTORY_START,
    };

    const AGENT: &str = "agent-1";
    const SESSION: &str = "sess-window";

    fn insert(conn: &rusqlite::Connection, log_type: &str, speaker: Option<&str>, content: &str) {
        let row = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: AGENT.into(),
            session_id: SESSION.into(),
            log_type: log_type.into(),
            content: content.into(),
            speaker_id: speaker.map(str::to_string),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(conn, &row).unwrap();
    }

    /// 窓より短い会話: 6 speech と 1 組の tool 行。
    fn seed_short_session() -> rusqlite::Connection {
        let conn = opencrab_db::init_memory().unwrap();
        for i in 0..6 {
            let speaker = if i % 2 == 0 { "owner" } else { AGENT };
            insert(&conn, "speech", Some(speaker), &format!("speech-{i:02}"));
            if i == 1 {
                insert(&conn, "tool_call", Some(AGENT), "call-01");
                insert(&conn, "tool_result", Some(AGENT), "result-01");
            }
        }
        conn
    }

    fn body(conversation: &str) -> &str {
        let open = conversation
            .find(CONVERSATION_HISTORY_START)
            .expect("history start");
        let close = conversation
            .find(CONVERSATION_HISTORY_END)
            .expect("history end");
        conversation[open + CONVERSATION_HISTORY_START.len()..close].trim_matches('\n')
    }

    /// `window: None` は既定の組立と 1 バイトも違わない（同じ関数・同じ引数）。
    #[test]
    fn window_none_is_the_default_conversation() {
        let conn = seed_short_session();
        let default = build_conversation_string_with_waters(&conn, SESSION, AGENT, 45_000, 20_000,
            false)
        .unwrap();
        let windowed = build_conversation_string_with_window(
            &conn,
            SESSION,
            AGENT,
            45_000,
            20_000,
            false,
            None,
        )
        .unwrap();
        assert_eq!(windowed, default);
    }

    /// 窓が会話より広ければ、履歴本文は既定の履歴と同じ行を同じ順で並べる。
    #[test]
    fn a_window_longer_than_the_session_renders_every_retained_row() {
        let conn = seed_short_session();
        let default = build_conversation_string_with_waters(&conn, SESSION, AGENT, 45_000, 20_000,
            false)
        .unwrap();
        let windowed = build_conversation_string_with_window(
            &conn,
            SESSION,
            AGENT,
            45_000,
            20_000,
            false,
            Some(ConversationWindow {
                speech_rows: 30,
                rows: RecentWindow::Live,
            }),
        )
        .unwrap();
        assert_eq!(body(&windowed), body(&default));
        for present in ["speech-00", "speech-05", "result-01"] {
            assert!(body(&windowed).contains(present), "{present}: {windowed}");
        }
    }
}
