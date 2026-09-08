fn web_inbound<'a>(
    session_id: &'a str,
    agent_id: &'a str,
    sender_id: &'a str,
    text: &'a str,
) -> NormalizedInbound<'a> {
    NormalizedInbound {
        session_id,
        agent_id,
        sender_id,
        sender_name: "",
        avatar_url: None,
        channel_id: None,
        pubkey: None,
        text,
        image_urls: &[],
        external_id: "",
    }
}

#[test]
fn prepare_session_inbound_write_ensures_then_records() {
    let calls = std::sync::Mutex::new(Vec::new());
    let inbound = web_inbound("web-a-c1", "a", "alice", "hi");
    prepare_session_inbound_write(
        &inbound,
        |sid, aid| {
            calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
            Ok(())
        },
        |aid, sid, uid, content| {
            calls
                .lock()
                .unwrap()
                .push(format!("record:{aid}:{sid}:{uid}:{content}"));
            Ok(())
        },
    )
    .expect("ensure+record は成功する");
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            "ensure:web-a-c1:a".to_string(),
            "record:a:web-a-c1:alice:hi".to_string(),
        ]
    );
}

#[test]
fn prepare_session_inbound_write_ensure_failure_skips_record() {
    let calls = std::sync::Mutex::new(Vec::new());
    match prepare_session_inbound_write(
        &web_inbound("s", "a", "u", "hi"),
        |sid, aid| {
            calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
            Err(anyhow::anyhow!("disk full"))
        },
        |_, _, _, _| {
            calls.lock().unwrap().push("record".into());
            Ok(())
        },
    ) {
        Err(PrepareSessionInboundError::Ensure(e)) => {
            assert!(e.to_string().contains("disk full"), "{e:#}");
        }
        other => panic!("expected Ensure, got {other:?}"),
    }
    assert_eq!(*calls.lock().unwrap(), vec!["ensure:s:a".to_string()]);
}

#[test]
fn prepare_session_inbound_write_record_failure_is_distinct() {
    let calls = std::sync::Mutex::new(Vec::new());
    match prepare_session_inbound_write(
        &web_inbound("s", "a", "u", "hi"),
        |sid, aid| {
            calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
            Ok(())
        },
        |aid, sid, uid, content| {
            calls
                .lock()
                .unwrap()
                .push(format!("record:{aid}:{sid}:{uid}:{content}"));
            Err(anyhow::anyhow!("locked"))
        },
    ) {
        Err(PrepareSessionInboundError::Record(e)) => {
            assert!(e.to_string().contains("locked"), "{e:#}");
        }
        other => panic!("expected Record, got {other:?}"),
    }
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["ensure:s:a".to_string(), "record:a:s:u:hi".to_string()]
    );
}
