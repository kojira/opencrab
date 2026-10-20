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

/// D-1081: `model_profile` is optional and strict; absent/null/"default" keep the default slot.
#[test]
fn said_model_profile_is_strict_and_optional() {
    use opencrab_actions::TurnModelProfile;
    let mut frame = serde_json::json!({
        "id": "said-1", "m": "said",
        "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "origin": "event-1", "author_id": "sender-1",
        "text": "hello", "attachments": []
    });
    let profile = |frame: &serde_json::Value| match parse_inbound(frame).unwrap() {
        InboundMsg::Said(said) => Some(said.model_profile),
        InboundMsg::Invalid {
            code: ErrorCode::BadRequest,
            ..
        } => None,
        _ => panic!("unexpected message"),
    };
    assert_eq!(profile(&frame), Some(TurnModelProfile::Default));
    frame["model_profile"] = json!("realtime");
    assert_eq!(profile(&frame), Some(TurnModelProfile::Realtime));
    frame["model_profile"] = json!("default");
    assert_eq!(profile(&frame), Some(TurnModelProfile::Default));
    frame["model_profile"] = serde_json::Value::Null;
    assert_eq!(profile(&frame), Some(TurnModelProfile::Default));
    for bad in [json!("bogus"), json!("Realtime"), json!(1), json!({})] {
        frame["model_profile"] = bad.clone();
        assert_eq!(profile(&frame), None, "{bad}");
    }
}

/// D-1085: `agent_prelude` is optional; a non-blank string of at most 64 chars is kept, anything
/// else is `bad_request`.
#[test]
fn agent_prelude_is_optional_and_bounded() {
    let frame = |prelude: Option<serde_json::Value>| {
        let mut frame = serde_json::json!({
            "id": "said-1", "m": "said",
            "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "origin": "event-1", "author_id": "sender-1",
            "text": "hello", "attachments": []
        });
        if let Some(prelude) = prelude {
            frame["agent_prelude"] = prelude;
        }
        frame
    };
    let prelude_of = |frame: &serde_json::Value| match parse_inbound(frame).unwrap() {
        InboundMsg::Said(said) => Ok(said.agent_prelude),
        InboundMsg::Invalid { code, .. } => Err(code),
        _ => panic!("expected said"),
    };
    assert_eq!(prelude_of(&frame(None)), Ok(None));
    assert_eq!(prelude_of(&frame(Some(serde_json::Value::Null))), Ok(None));
    assert_eq!(
        prelude_of(&frame(Some(serde_json::json!("うん、")))),
        Ok(Some("うん、".to_string()))
    );
    let longest = "あ".repeat(MAX_AGENT_PRELUDE_CHARS);
    assert_eq!(
        prelude_of(&frame(Some(serde_json::json!(longest)))),
        Ok(Some(longest.clone()))
    );
    for bad in [
        serde_json::json!("あ".repeat(MAX_AGENT_PRELUDE_CHARS + 1)),
        serde_json::json!(""),
        serde_json::json!("  "),
        serde_json::json!(1),
        serde_json::json!(["うん、"]),
    ] {
        assert_eq!(
            prelude_of(&frame(Some(bad.clone()))),
            Err(ErrorCode::BadRequest),
            "{bad}"
        );
    }
}

// D-1087: the activity frames are id-less fire-and-forget notifications. A gateway that does
// not know them drops them with zero writes and keeps the connection, so the shape is pinned
// here: `m` + `binding_id` + the neutral fields, and no `id`.

#[test]
fn tool_activity_started_frame_carries_no_id_and_no_terminal_fields() {
    let args = serde_json::json!({"cmd": "ls"});
    let frame = tool_activity_frame(
        "binding",
        "activity",
        "call-1",
        "execute_shell",
        "started",
        2,
        "2026-10-20T00:00:00Z",
        None,
        &args,
        None,
        None,
    );
    assert_eq!(
        frame,
        serde_json::json!({
            "m": "tool_activity",
            "binding_id": "binding",
            "activity_id": "activity",
            "call_id": "call-1",
            "tool": "execute_shell",
            "status": "started",
            "depth": 2,
            "started_at": "2026-10-20T00:00:00Z",
            "args": {"cmd": "ls"},
        })
    );
    assert!(frame.get("id").is_none());
}

#[test]
fn tool_activity_terminal_frame_adds_duration_result_and_error_only_when_present() {
    let args = serde_json::json!({});
    let result = serde_json::json!({"ok": true});
    let completed = tool_activity_frame(
        "binding",
        "activity",
        "call-1",
        "execute_shell",
        "completed",
        0,
        "2026-10-20T00:00:00Z",
        Some(12),
        &args,
        Some(&result),
        None,
    );
    assert_eq!(completed["duration_ms"], 12);
    assert_eq!(completed["result"], serde_json::json!({"ok": true}));
    assert!(completed.get("error").is_none());

    let rejected = tool_activity_frame(
        "binding",
        "activity",
        "call-2",
        "execute_shell",
        "rejected",
        0,
        "2026-10-20T00:00:00Z",
        Some(0),
        &args,
        None,
        Some("rejected: owner"),
    );
    assert_eq!(rejected["error"], "rejected: owner");
    assert!(rejected.get("result").is_none());
    assert!(rejected.get("id").is_none());
}

#[test]
fn subtask_lifecycle_frame_omits_the_fields_that_do_not_apply() {
    let started = subtask_lifecycle_frame(
        "binding",
        "run-1",
        "started",
        "shell job",
        "do it",
        None,
        None,
        None,
    );
    assert_eq!(
        started,
        serde_json::json!({
            "m": "subtask_lifecycle",
            "binding_id": "binding",
            "run_id": "run-1",
            "status": "started",
            "label": "shell job",
            "task": "do it",
        })
    );
    assert!(started.get("id").is_none());

    let completed = subtask_lifecycle_frame(
        "binding",
        "run-1",
        "completed",
        "",
        "",
        None,
        Some(42),
        Some("done"),
    );
    assert!(completed.get("label").is_none());
    assert!(completed.get("task").is_none());
    assert_eq!(completed["duration_ms"], 42);
    assert_eq!(completed["result_text"], "done");

    let progress = subtask_lifecycle_frame(
        "binding",
        "run-1",
        "progress",
        "shell job",
        "",
        Some("half way"),
        None,
        None,
    );
    assert_eq!(progress["detail"], "half way");
    assert!(progress.get("duration_ms").is_none());
}
