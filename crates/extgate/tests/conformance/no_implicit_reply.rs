// D-1093: core makes no reply-destination decision. A plain say never carries a `reply_target`:
// not the origin of the said that started the turn (the D-1067 default), not the said's own
// `reply_target` (display metadata), and not on continuation speech. A reply happens only when
// the model calls the gateway's explicit reply operation with a target.

async fn says_until_two(s: &mut tokio::net::UnixStream) -> Vec<serde_json::Value> {
    let mut says = Vec::new();
    for _ in 0..80 {
        if let Some(v) = read_frame_opt(s).await {
            if v["m"] == "say" {
                write_frame(s, &json!({"id": v["id"], "m": "ok"})).await;
                says.push(v);
                if says.len() == 2 {
                    break;
                }
            }
        }
    }
    says
}

#[tokio::test]
async fn plain_say_never_carries_a_reply_target() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "nir-1", "m": "said", "session": binding_id,
            "origin": "nir-origin", "author_id": "u1",
            "reply_target": "nir-parent",
            "text": "hi", "attachments": []
        }),
    )
    .await;
    let says = says_until_two(&mut s).await;
    assert!(!says.is_empty(), "no say");
    for say in &says {
        assert!(
            say["payload"].get("reply_target").is_none(),
            "core attached a reply destination to a plain say: {say}"
        );
    }
    assert_eq!(
        h.runtime.reply_targets.lock().unwrap().last().cloned(),
        Some(None),
        "the turn request must carry no implicit reply destination"
    );
}

#[tokio::test]
async fn continuation_speech_never_carries_a_reply_target() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.partial_script.set(vec![
        PartialStep::Begin("nir-stream"),
        PartialStep::Say("first line"),
    ]);
    write_frame(&mut s, &said_frame_with_profile("nir-2", &binding_id, "nir-origin-2", Some("realtime")))
        .await;
    assert_eq!(read_said_response(&mut s, "nir-2").await["m"], "ok");
    let says: Vec<Value> = turn_frames(&mut s)
        .await
        .into_iter()
        .filter(|f| f["m"] == "say")
        .collect();
    assert_eq!(says.len(), 2, "continuation + final say expected: {says:?}");
    for say in &says {
        assert!(
            say["payload"].get("reply_target").is_none(),
            "core attached a reply destination to a say: {say}"
        );
    }
}
