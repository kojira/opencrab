// D-1085: `said.agent_prelude` is recorded as the agent's own speech right after the said, in the
// same origin transaction, and is never delivered back as a say.

/// `(log_type, speaker_id, content, metadata source)` of every row of the session, oldest first.
fn session_rows(h: &Harness, session_id: &str) -> Vec<(String, Option<String>, String, Option<String>)> {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
        .unwrap()
        .into_iter()
        .map(|row| {
            let source = row
                .metadata_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|meta| meta["source"].as_str().map(str::to_string));
            (row.log_type, row.speaker_id, row.content, source)
        })
        .collect()
}

fn said_with_prelude(id: &str, binding_id: &str, prelude: Option<&str>) -> Value {
    let mut frame = json!({
        "id": id, "m": "said", "binding_id": binding_id,
        "origin": format!("o-{id}"), "author_id": "u1",
        "model_profile": "realtime",
        "text": "hello", "attachments": []
    });
    if let Some(prelude) = prelude {
        frame["agent_prelude"] = json!(prelude);
    }
    frame
}

#[tokio::test]
async fn agent_prelude_is_recorded_after_the_said_and_not_delivered() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);

    write_frame(&mut s, &said_with_prelude("p1", &binding_id, Some("うん、"))).await;
    assert_eq!(read_said_response(&mut s, "p1").await["m"], "ok");
    wait_turns(&h, 1).await;

    assert_eq!(
        session_rows(&h, &session_id),
        [
            (
                "speech".to_string(),
                Some("u1".to_string()),
                "hello".to_string(),
                Some("external".to_string())
            ),
            (
                "speech".to_string(),
                Some("agent-1".to_string()),
                "うん、".to_string(),
                Some("external_response".to_string())
            ),
        ]
    );
    // The turn already sees the prelude as the line right after the said.
    let conversation = h.runtime.conversations.lock().unwrap().last().cloned().unwrap();
    assert!(
        conversation.ends_with("speech:hello\nspeech:うん、"),
        "{conversation}"
    );
    // Never delivered: no say frame and no delivery row.
    while let Some(frame) = read_frame_opt(&mut s).await {
        assert_ne!(frame["m"], "say", "{frame}");
    }
    let deliveries: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(deliveries, 0);
}

#[tokio::test]
async fn said_without_agent_prelude_records_only_the_said() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);

    write_frame(&mut s, &said_with_prelude("p1", &binding_id, None)).await;
    assert_eq!(read_said_response(&mut s, "p1").await["m"], "ok");
    wait_turns(&h, 1).await;

    assert_eq!(
        session_rows(&h, &session_id),
        [(
            "speech".to_string(),
            Some("u1".to_string()),
            "hello".to_string(),
            Some("external".to_string())
        )]
    );
}

#[tokio::test]
async fn oversized_agent_prelude_is_bad_request_and_records_nothing() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);

    let long = "あ".repeat(65);
    write_frame(&mut s, &said_with_prelude("p1", &binding_id, Some(&long))).await;
    assert_eq!(read_said_response(&mut s, "p1").await["code"], "bad_request");
    assert!(session_rows(&h, &session_id).is_empty());
}
