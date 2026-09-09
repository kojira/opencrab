#[cfg(test)]
mod pending_completion_history_tests {
    use super::build_conversation_string;

    fn insert(
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        log_type: &str,
        content: &str,
        speaker_id: Option<&str>,
    ) {
        opencrab_db::queries::insert_session_log_best_effort(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker_id.map(str::to_string),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        );
    }

    #[test]
    fn completion_body_is_present_until_agent_answers_then_becomes_reference() {
        let conn = opencrab_db::init_memory().unwrap();
        let agent = "agent-history";
        let session = "session-history";
        insert(&conn, agent, session, "speech", "run it", Some("owner"));
        insert(
            &conn,
            agent,
            session,
            "system",
            &serde_json::json!({
                "type": "subtask_completed",
                "subtask_id": "subtask-1",
                "session_id": "subtask-subtask-1",
                "exit_reason": "completed",
                "result": "issue975-exact-result"
            })
            .to_string(),
            None,
        );

        // 同じorigin/subtaskの重複通知は最新1件だけを会話へ載せる。
        insert(
            &conn,
            agent,
            session,
            "system",
            &serde_json::json!({
                "type": "subtask_completed",
                "subtask_id": "subtask-1",
                "session_id": "subtask-subtask-1",
                "exit_reason": "completed",
                "result": "issue975-exact-result"
            })
            .to_string(),
            None,
        );

        let pending = build_conversation_string(&conn, session, agent, 20_000).unwrap();
        assert_eq!(pending.matches("issue975-exact-result").count(), 1);
        // 応答保存前のrestart/replayでは同じ保存ログから同じrequestを再構築できる。
        assert_eq!(
            build_conversation_string(&conn, session, agent, 20_000).unwrap(),
            pending
        );

        // 他者（botを含む）の発言はcompletionへの自分の応答ではない。
        insert(&conn, agent, session, "speech", "bot interjection", Some("other-bot"));
        let still_pending = build_conversation_string(&conn, session, agent, 20_000).unwrap();
        assert!(still_pending.contains("issue975-exact-result"));

        insert(&conn, agent, session, "speech", "reported result", None);
        let answered = build_conversation_string(&conn, session, agent, 20_000).unwrap();
        assert!(!answered.contains("issue975-exact-result"));
        assert!(answered.contains("result_omitted:true"));
        assert!(answered.contains("read_my_history(around_id="));
    }

    #[test]
    fn completed_plain_json_result_is_not_mislabeled_failed() {
        let conn = opencrab_db::init_memory().unwrap();
        let result = serde_json::json!({"answer": "done"}).to_string();
        insert(
            &conn,
            "agent-json",
            "session-json",
            "system",
            &serde_json::json!({
                "type": "subtask_completed",
                "subtask_id": "subtask-json",
                "exit_reason": "completed",
                "result": result
            })
            .to_string(),
            None,
        );

        let conversation =
            build_conversation_string(&conn, "session-json", "agent-json", 20_000).unwrap();
        assert!(conversation.contains("status:completed"));
        assert!(!conversation.contains("status:failed"));
    }
}
