#[tokio::test]
async fn said_dedup_same_origin_and_separate_bindings() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_a) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_a,
            "origin": "same",
            "author_id": "u1",
            "text": "one",
            "attachments": []
        }),
    )
    .await;
    let first = read_said_response(&mut s, "s1").await;
    assert_eq!(first["seq"], 1);
    let turns = h.runtime.turns.load(Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s2",
            "m": "said",
            "binding_id": binding_a,
            "origin": "same",
            "author_id": "u1",
            "text": "two",
            "attachments": []
        }),
    )
    .await;
    let again = read_said_response(&mut s, "s2").await;
    assert_eq!(again["seq"], 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), turns);

    let binding_b = uuid();
    put_binding(&h, &binding_b, &instance_id, "chan-b").await;
    let bind = read_frame(&mut s).await;
    assert_eq!(bind["binding_id"], binding_b);
    write_frame(&mut s, &json!({"id": bind["id"], "m": "ok"})).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s3",
            "m": "said",
            "binding_id": binding_b,
            "origin": "same",
            "author_id": "u1",
            "text": "other",
            "attachments": []
        }),
    )
    .await;
    let other = read_said_response(&mut s, "s3").await;
    assert_eq!(other["seq"], 1);
}

#[tokio::test]
async fn said_seq_null_when_not_recorded_and_lookups_are_real() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    *h.state.probe.whitelist_override.lock().unwrap() = Some(false);
    let accepts = h.state.probe.accept_inbound_count.load(Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "drop",
            "author_id": "u1",
            "text": "no",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["m"], "ok");
    assert!(v["seq"].is_null());
    assert!(h.state.probe.accept_inbound_count.load(Ordering::SeqCst) > accepts);
    assert!(h.state.probe.lookup_wl_count.load(Ordering::SeqCst) > 0);
    *h.state.probe.whitelist_override.lock().unwrap() = None;
}

#[tokio::test]
async fn empty_said_is_bad_request_no_record() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "e",
            "author_id": "u",
            "text": "",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "bad_request");
}

#[tokio::test]
async fn local_html_is_verified_recorded_and_promoted() {
    use sha2::Digest as _;

    let h = Harness::start().await;
    let (mut s, instance_id, binding_id) = ready_pair(&h).await;
    let attachments = tempfile::tempdir().unwrap();
    let inbox = attachments.path().join("inbox");
    let relative = format!("{instance_id}/origin/attachment.bin");
    let source = inbox.join(&relative);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let body = b"<html><body>GPU OOM explanation</body></html>";
    std::fs::write(&source, body).unwrap();
    h.state
        .set_attachment_inbox_root(inbox.canonicalize().unwrap());
    let hash = format!("{:x}", sha2::Sha256::digest(body));

    write_frame(
        &mut s,
        &json!({
            "id": "local-1", "m": "said", "binding_id": binding_id,
            "origin": "local-html", "author_id": "u1", "text": "",
            "attachments": [{
                "kind": "file",
                "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "name": "page.html", "media_type": "text/html",
                "size": body.len(), "sha256": hash, "local_path": relative
            }]
        }),
    )
    .await;
    let response = read_said_response(&mut s, "local-1").await;
    assert_eq!(response["seq"], 1);
    assert!(!source.exists());
    assert!(attachments
        .path()
        .join(format!("store/{hash}.bin"))
        .is_file());
    let conn = h.state.db.lock().unwrap();
    let (content, metadata): (String, String) = conn
        .query_row(
            "SELECT content, metadata_json FROM memory_sessions WHERE json_extract(metadata_json, '$.external_origin') = 'local-html' ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(content.contains("GPU OOM explanation"));
    assert!(!metadata.contains("https://"));
    assert!(!metadata.contains(attachments.path().to_string_lossy().as_ref()));
    assert!(metadata.contains(&format!("store/{hash}.bin")));
    drop(conn);

    // Reusing a gateway-supplied UUID with different content must not replace
    // the first retained original; storage identity is the verified hash.
    let relative2 = format!("{instance_id}/origin/attachment-2.bin");
    let source2 = inbox.join(&relative2);
    let body2 = b"<html><body>different</body></html>";
    std::fs::write(&source2, body2).unwrap();
    let hash2 = format!("{:x}", sha2::Sha256::digest(body2));
    write_frame(
        &mut s,
        &json!({
            "id": "local-2", "m": "said", "binding_id": binding_id,
            "origin": "local-html-2", "author_id": "u1", "text": "",
            "attachments": [{
                "kind": "file", "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "name": "other.html", "media_type": "text/html",
                "size": body2.len(), "sha256": hash2, "local_path": relative2
            }]
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "local-2").await["seq"], 2);
    assert_eq!(
        std::fs::read(attachments.path().join(format!("store/{hash}.bin"))).unwrap(),
        body
    );
    assert_eq!(
        std::fs::read(attachments.path().join(format!("store/{hash2}.bin"))).unwrap(),
        body2
    );
}

#[tokio::test]
async fn mixed_local_attachments_reach_turn_in_declared_order() {
    use sha2::Digest as _;

    let h = Harness::start().await;
    let (mut s, instance_id, binding_id) = ready_pair(&h).await;
    let attachments = tempfile::tempdir().unwrap();
    let inbox = attachments.path().join("inbox");
    let image_relative = format!("{instance_id}/origin/image.bin");
    let image_source = inbox.join(&image_relative);
    std::fs::create_dir_all(image_source.parent().unwrap()).unwrap();
    let image = b"\x89PNG\r\n\x1a\nimage";
    std::fs::write(&image_source, image).unwrap();
    let text_relative = format!("{instance_id}/origin/note.bin");
    let text_source = inbox.join(&text_relative);
    let text = b"first attachment text";
    std::fs::write(&text_source, text).unwrap();
    h.state
        .set_attachment_inbox_root(inbox.canonicalize().unwrap());
    let image_hash = format!("{:x}", sha2::Sha256::digest(image));
    let text_hash = format!("{:x}", sha2::Sha256::digest(text));

    write_frame(
        &mut s,
        &json!({
            "id": "local-image", "m": "said", "binding_id": binding_id,
            "origin": "local-image", "author_id": "u1", "text": "",
            "attachments": [
                {
                    "kind": "file", "id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
                    "name": "note.txt", "media_type": "text/plain",
                    "size": text.len(), "sha256": text_hash, "local_path": text_relative
                },
                {
                    "kind": "file", "id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                    "name": "image.png", "media_type": "image/png",
                    "size": image.len(), "sha256": image_hash, "local_path": image_relative
                }
            ]
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "local-image").await["seq"], 1);
    for _ in 0..50 {
        if !h.runtime.images.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let images = h.runtime.images.lock().unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].len(), 1);
    assert!(images[0][0].starts_with("data:image/png;base64,"));
    drop(images);
    let conversations = h.runtime.conversations.lock().unwrap();
    let conversation = conversations.last().unwrap();
    assert!(conversation.contains("1. note.txt"));
    assert!(conversation.contains("2. image.png"));
    assert!(conversation.contains("first attachment text"));
    assert!(attachments
        .path()
        .join(format!("store/{image_hash}.bin"))
        .is_file());
    assert!(attachments
        .path()
        .join(format!("store/{text_hash}.bin"))
        .is_file());
}

#[tokio::test]
async fn image_only_said_is_recorded_and_starts_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let before = h.runtime.turns.load(Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "img",
            "author_id": "u1",
            "text": "",
            "attachments": [{"kind":"image","url":"https://example.com/a.png"}]
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["seq"], 1);
    for _ in 0..50 {
        if h.runtime.turns.load(Ordering::SeqCst) > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(h.runtime.turns.load(Ordering::SeqCst) > before);
    assert_eq!(
        h.state
            .probe
            .start_session_turn_count
            .load(Ordering::SeqCst),
        h.runtime.turns.load(Ordering::SeqCst)
    );
}

