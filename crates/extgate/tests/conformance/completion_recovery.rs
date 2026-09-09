#[tokio::test]
async fn bind_ack_resumes_pending_response_effect_after_process_restart() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-effect-recovery").await;
    let session_id = session_id_for_binding(&binding_id);
    {
        let conn = h.state.db.lock().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            session_id: session_id.clone(),
            agent_id: "agent-1".to_string(),
            log_type: "tool_result".to_string(),
            content: "[<t1] status:completed\nrecovered result".to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let result_log_id = opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
        opencrab_db::queries::enqueue_tool_completion_event(
            &conn,
            &opencrab_db::queries::NewToolCompletionEvent {
                event_id: "event-effect-recovery",
                session_id: &session_id,
                causal_turn_id: "turn-before-restart",
                tool_call_id: "t1",
                execution_id: "exec-effect-before-restart",
                result_log_id,
                completed_at: "2026-09-08T00:00:00Z",
            },
        )
        .unwrap();
        let ids = vec!["event-effect-recovery".to_string()];
        opencrab_db::queries::mark_tool_completion_events_included(
            &conn,
            &ids,
            "request-effect-recovery",
            "digest",
            r#"{"model":"test","messages":[]}"#,
        )
        .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        opencrab_db::queries::record_tool_completion_response_in_transaction(
            &tx,
            "request-effect-recovery",
            r#"{"choices":[]}"#,
        )
        .unwrap();
        opencrab_db::queries::mark_tool_completion_events_consumed_in_transaction(
            &tx,
            &ids,
            "request-effect-recovery",
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let mut socket = h.connect().await;
    hello_ok(&mut socket, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut socket).await, binding_id);
    for _ in 0..100 {
        if h.runtime.turns.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bind_ack_resumes_completion_persisted_before_process_restart() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-recovery").await;
    let session_id = session_id_for_binding(&binding_id);
    {
        let conn = h.state.db.lock().unwrap();
        let metadata = json!({
            "conversation_tool_id": "t1",
            "lifecycle_status": "completed"
        })
        .to_string();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            session_id: session_id.clone(),
            agent_id: "agent-1".to_string(),
            log_type: "tool_result".to_string(),
            content: "[<t1] status:completed\nrecovered result".to_string(),
            speaker_id: None,
            turn_number: None,
            metadata_json: Some(metadata),
            created_at: None,
        };
        let result_log_id = opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
        opencrab_db::queries::enqueue_tool_completion_event(
            &conn,
            &opencrab_db::queries::NewToolCompletionEvent {
                event_id: "event-recovery",
                session_id: &session_id,
                causal_turn_id: "turn-before-restart",
                tool_call_id: "t1",
                execution_id: "exec-before-restart",
                result_log_id,
                completed_at: "2026-09-08T00:00:00Z",
            },
        )
        .unwrap();
    }

    let mut socket = h.connect().await;
    hello_ok(&mut socket, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut socket).await, binding_id);
    for _ in 0..100 {
        if h.runtime.turns.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
    let conversations = h.runtime.conversations.lock().unwrap();
    assert!(conversations[0].contains("recovered result"));
}
