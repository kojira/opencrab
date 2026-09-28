#![cfg(test)]

use opencrab_actions::{AgentRuntime, InboundMessageRecord, TranscriptSource};

#[test]
fn inbound_speech_does_not_create_peer_review_progress_after_legacy_identity_cleanup() {
    let state = crate::test_app_state();
    let source = TranscriptSource::new("external", "test-out");
    let session_id = "s1";
    let ledger_id = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "a1".into(),
                name: "Agent".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "agent".into(),
                personality: None,
                instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
        let id = opencrab_db::queries::insert_task_ledger(&conn, "a1", session_id, "goal", None)
            .unwrap();
        opencrab_db::queries::insert_task_progress(
            &conn,
            id,
            "progress",
            "[peer review requested] posted to channel 1 (1 parts)",
        )
        .unwrap();
        id
    };
    state.ensure_session(session_id, &["a1".into()], "", "{}", "discord");

    assert!(!state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='trusted_users')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap());
    for text in [
        "[Peer Review] score: 0.6 summary: ordinary speech",
        "[Peer Review] score: 0.7 summary: second speech",
    ] {
        let record = InboundMessageRecord {
            session_id,
            recipient_agent_id: "a1",
            sender_id: "42",
            sender_name: "Reviewer",
            avatar_url: None,
            channel_id: Some("1"),
            pubkey: None,
            text,
            image_urls: &[],
        };
        assert!(state.record_inbound_message(source, &record));
        state.on_inbound_message(source, "a1", &record);
        let conn = state.db.lock().unwrap();
        let progress =
            opencrab_db::queries::list_recent_task_progress(&conn, ledger_id, 10).unwrap();
        assert_eq!(
            progress.len(),
            1,
            "removed peer review must not change task progress"
        );
        assert!(
            opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
                .unwrap()
                .iter()
                .any(|row| row.content == text),
            "inbound speech must still be stored"
        );
    }
}
