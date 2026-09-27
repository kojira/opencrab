use super::*;

#[test]
fn completed_target_is_additive_and_conditional() {
    let ended = activity_frame("binding", "activity", "ended", None, Some("utterance"));
    assert_eq!(ended["completed_target"], "utterance");
    assert!(ended.get("origin").is_none());

    let read = activity_frame("binding", "activity", "read", Some("origin"), None);
    assert_eq!(read["origin"], "origin");
    assert!(read.get("completed_target").is_none());
}

#[test]
fn authoritative_ended_always_carries_silent_origins_and_can_coexist() {
    let empty = ended_activity_frame("binding", "activity", None, &[]);
    assert_eq!(empty["silent_origins"], serde_json::json!([]));
    assert!(empty.get("completed_target").is_none());

    let ended = ended_activity_frame(
        "binding",
        "activity",
        Some("utterance"),
        &["origin-b".to_string()],
    );
    assert_eq!(ended["completed_target"], "utterance");
    assert_eq!(ended["silent_origins"], serde_json::json!(["origin-b"]));
}

#[test]
fn parses_provider_neutral_local_attachment() {
    let frame = serde_json::json!({
        "id": "said-1",
        "m": "said",
        "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "origin": "event-1",
        "author_id": "sender-1",
        "text": "",
        "attachments": [{
            "kind": "file",
            "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "name": "page.html",
            "media_type": "text/html",
            "size": 42,
            "sha256": "a".repeat(64),
            "local_path": "instance/origin/file.bin"
        }]
    });
    let InboundMsg::Said(said) = parse_inbound(&frame).unwrap() else {
        panic!("expected said");
    };
    assert!(matches!(
        &said.attachments[0],
        SaidAttachment::LocalFile { name, size: 42, .. } if name == "page.html"
    ));
}

#[test]
fn author_label_is_optional_display_metadata() {
    let mut frame = serde_json::json!({
        "id": "said-1", "m": "said",
        "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "origin": "event-1", "author_id": "sender-1", "author_label": "Alice",
        "text": "hello", "attachments": []
    });
    let InboundMsg::Said(said) = parse_inbound(&frame).unwrap() else {
        panic!("expected said");
    };
    assert_eq!(said.author_id, "sender-1");
    assert_eq!(said.author_label.as_deref(), Some("Alice"));

    frame.as_object_mut().unwrap().remove("author_label");
    let InboundMsg::Said(said) = parse_inbound(&frame).unwrap() else {
        panic!("expected said");
    };
    assert_eq!(said.author_label, None);

    frame["author_label"] = serde_json::json!("bad\nlabel");
    assert!(matches!(
        parse_inbound(&frame).unwrap(),
        InboundMsg::Invalid {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
}

#[test]
fn command_requires_an_object_caller() {
    for caller in [
        serde_json::Value::Null,
        serde_json::json!("owner"),
        serde_json::json!(true),
        serde_json::json!([]),
    ] {
        let frame = serde_json::json!({
            "id": "command-1",
            "m": "command",
            "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "caller": caller,
            "name": "list_models",
            "args": {}
        });
        assert!(matches!(
            parse_inbound(&frame).unwrap(),
            InboundMsg::Invalid {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
    }
}

#[test]
fn rejects_local_attachment_path_traversal() {
    let mut frame = serde_json::json!({
        "id": "said-1", "m": "said",
        "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "origin": "event-1", "author_id": "sender-1", "text": "",
        "attachments": [{
            "kind": "file", "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            "name": "page.html", "media_type": "text/html", "size": 42,
            "sha256": "a".repeat(64), "local_path": "../escape"
        }]
    });
    assert!(matches!(
        parse_inbound(&frame).unwrap(),
        InboundMsg::Invalid {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
    frame["attachments"][0]["local_path"] = json!("/absolute/file");
    assert!(matches!(
        parse_inbound(&frame).unwrap(),
        InboundMsg::Invalid {
            code: ErrorCode::BadRequest,
            ..
        }
    ));
}
