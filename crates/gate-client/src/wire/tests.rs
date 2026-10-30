use super::*;

#[test]
fn said_author_label_is_additive_and_optional() {
    let legacy = said_frame("said:1", "binding", "origin", "author", "hello", &[]);
    assert!(legacy.get("author_label").is_none());
    let labeled = said_frame_with_author_label(
        "said:1",
        "binding",
        "origin",
        "author",
        Some("Alice"),
        "hello",
        &[],
    );
    assert_eq!(labeled["author_id"], "author");
    assert_eq!(labeled["author_label"], "Alice");
}

/// D-1081: `model_profile` is emitted only for a non-default profile.
#[test]
fn said_model_profile_is_sent_only_when_not_default() {
    let mut context = SaidContext {
        caller: SaidCaller::Owner,
        start_turn: true,
        system_context: None,
        reply_target: None,
        reference: None,
        live_inbound_scope: LiveInboundScope::All,
        model_profile: TurnModelProfile::Default,
        agent_prelude: None,
        provisional_id: None,
    };
    let frame = |context: &SaidContext| {
        said_frame_with_context("said:1", "b", "o", "a", None, Some(context), "hi", &[])
    };
    assert!(frame(&context).get("model_profile").is_none());
    context.model_profile = TurnModelProfile::Realtime;
    assert_eq!(frame(&context)["model_profile"], "realtime");
}

#[test]
fn bind_is_an_unknown_frame_now() {
    // D-1099: core never sends `bind`; a gateway treats one like any unknown frame.
    let raw = br#"{"id":"bind:x","m":"bind","binding_id":"x","address":"a"}"#;
    assert!(matches!(
        parse_frame_bytes(raw).unwrap(),
        CoreMsg::Unknown { .. }
    ));
}

#[test]
fn say_text_ignores_unknown_and_rejects_empty() {
    assert_eq!(say_text(&json!({"text":"hi","extra":1})), Some("hi"));
    assert_eq!(say_text(&json!({"text":""})), None);
    assert_eq!(say_text(&json!({})), None);
}

#[test]
fn say_reply_target_reads_optional_origin() {
    assert_eq!(
        say_reply_target(&json!({"text":"hi","reply_target":"opaque-origin"})),
        Ok(Some("opaque-origin"))
    );
    assert_eq!(say_reply_target(&json!({"text":"hi"})), Ok(None));
    for reply_target in [Value::Null, json!(42), json!("")] {
        assert_eq!(
            say_reply_target(&json!({"text":"hi","reply_target":reply_target})),
            Err(FrameError::BadRequest)
        );
    }
}

#[test]
fn malformed_present_say_reply_target_is_bad_request() {
    for reply_target in [Value::Null, json!(42), json!("")] {
        let raw = serde_json::to_vec(&json!({
            "id": "say:1",
            "m": "say",
            "session": "room",
            "payload": {"text": "hi", "reply_target": reply_target}
        }))
        .unwrap();
        match parse_frame_bytes(&raw).unwrap() {
            CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
            other => panic!("malformed reply_target accepted: {other:?}"),
        }
    }
}

#[test]
fn duplicate_member_is_bad_request() {
    let raw = br#"{"id":"1","m":"ok","id":"2"}"#;
    assert_eq!(parse_frame_bytes(raw).unwrap_err(), FrameError::BadRequest);
}

#[test]
fn parse_invoke_ok() {
    let raw = br#"{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"invoke","invocation_protocol":1,"session":"room","declaration_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","operation":"reply","dispatch":"utterance","effect":"utterance","context":{"continuation_id":null},"payload":{"event":"e7","text":"hi"}}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Invoke(i) => {
            assert_eq!(i.id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
            assert_eq!(i.session_key, "room");
            assert_eq!(i.declaration_digest.len(), 64);
            assert_eq!(i.operation, "reply");
            assert_eq!(i.dispatch, "utterance");
            assert_eq!(i.effect, "utterance");
            assert_eq!(i.continuation_id, None);
            assert_eq!(i.payload, json!({"event":"e7","text":"hi"}));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn s3_invoke_requires_digest_dispatch_and_effect() {
    let base = json!({
        "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "m": "invoke",
        "invocation_protocol": 1,
        "binding_id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "declaration_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "operation": "reply",
        "dispatch": "utterance",
        "effect": "utterance",
        "context": {"continuation_id": null},
        "payload": {"event": "e7", "text": "hi"}
    });
    for required in ["declaration_digest", "dispatch", "effect"] {
        let mut frame = base.clone();
        frame.as_object_mut().unwrap().remove(required);
        match parse_frame_bytes(&serde_json::to_vec(&frame).unwrap()).unwrap() {
            CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request", "{required}"),
            other => panic!("missing {required} accepted: {other:?}"),
        }
    }
}

#[test]
fn parse_invoke_missing_payload_is_bad_request() {
    let raw = br#"{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"invoke","session":"room","operation":"reply","context":{"continuation_id":null}}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
        other => panic!("{other:?}"),
    }
}

// #964: read は次の request に新しく含める投稿の origin を運ぶ。
#[test]
fn parse_activity_read_carries_origin() {
    let raw = br#"{"m":"activity","session":"room","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"read","origin":"omo-1"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Activity(a) => {
            assert_eq!(a.state, "read");
            assert_eq!(a.origin.as_deref(), Some("omo-1"));
            assert_eq!(a.completed_target, None);
        }
        other => panic!("{other:?}"),
    }
}

// origin 欠落は None（後方互換）。additive の未知 field も無視。
#[test]
fn parse_activity_without_origin_is_none() {
    let raw = br#"{"m":"activity","session":"room","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"ended","future_field":42}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Activity(a) => {
            assert_eq!(a.state, "ended");
            assert_eq!(a.origin, None);
            assert_eq!(a.completed_target, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_activity_ended_carries_completed_target() {
    let raw = br#"{"m":"activity","session":"room","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"ended","completed_target":"cccccccc-cccc-4ccc-8ccc-cccccccccccc"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Activity(a) => {
            assert_eq!(a.state, "ended");
            assert_eq!(a.origin, None);
            assert_eq!(
                a.completed_target.as_deref(),
                Some("cccccccc-cccc-4ccc-8ccc-cccccccccccc")
            );
            assert_eq!(a.silent_origins, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_activity_distinguishes_absent_empty_and_present_silent_origins() {
    for (field, expected) in [
        ("", None),
        (",\"silent_origins\":[]", Some(Vec::<String>::new())),
        (
            ",\"silent_origins\":[\"origin-a\",\"origin-b\"]",
            Some(vec!["origin-a".into(), "origin-b".into()]),
        ),
    ] {
        let raw = format!(
            "{{\"m\":\"activity\",\"session\":\"room\",\"activity_id\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"state\":\"ended\"{field}}}"
        );
        match parse_frame_bytes(raw.as_bytes()).unwrap() {
            CoreMsg::Activity(activity) => assert_eq!(activity.silent_origins, expected),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn malformed_silent_origins_is_bad_request() {
    for value in ["null", "{}", "[7]", "[\"\"]"] {
        let raw = format!(
            "{{\"m\":\"activity\",\"session\":\"room\",\"activity_id\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"state\":\"ended\",\"silent_origins\":{value}}}"
        );
        assert!(matches!(
            parse_frame_bytes(raw.as_bytes()).unwrap(),
            CoreMsg::Invalid {
                code: "bad_request",
                ..
            }
        ));
    }
}

// R3(❌): turn_failed は session + origin を運ぶ。error 本文（未知 field）は無視。
#[test]
fn parse_turn_failed_ok() {
    let raw = br#"{"m":"turn_failed","session":"room","origin":"boom-1","error":"leaked-should-be-ignored"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::TurnFailed(t) => {
            assert_eq!(t.session_key, "room");
            assert_eq!(t.origin, "boom-1");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_turn_failed_missing_origin_is_bad_request() {
    let raw = br#"{"m":"turn_failed","session":"room"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
        other => panic!("{other:?}"),
    }
}

// D-1087: 活動の 2 frame。id を持たないので応答対象にならず、値だけを運ぶ。
#[test]
fn tool_activity_frame_parses_started_and_terminal_fields() {
    let started = br#"{"m":"tool_activity","session":"room","activity_id":"act-1","call_id":"call-1","tool":"execute_shell","status":"started","depth":0,"started_at":"2026-10-20T00:00:00Z","args":{"command":"ls"}}"#;
    match parse_frame_bytes(started).unwrap() {
        CoreMsg::ToolActivity(a) => {
            assert_eq!(a.activity_id, "act-1");
            assert_eq!(a.status, "started");
            assert_eq!(a.args["command"], "ls");
            assert_eq!(a.duration_ms, None);
            assert_eq!(a.result, None);
            assert_eq!(a.error, None);
        }
        other => panic!("{other:?}"),
    }
    let terminal = br#"{"m":"tool_activity","session":"room","activity_id":"act-1","call_id":"call-1","tool":"execute_shell","status":"completed","depth":0,"started_at":"2026-10-20T00:00:00Z","duration_ms":12,"args":{"command":"ls"},"result":{"exit_code":0}}"#;
    match parse_frame_bytes(terminal).unwrap() {
        CoreMsg::ToolActivity(a) => {
            assert_eq!(a.duration_ms, Some(12));
            assert_eq!(a.result.unwrap()["exit_code"], 0);
        }
        other => panic!("{other:?}"),
    }
}

/// D-1087: 入れ子の走行で起きたツール実行は、ライフサイクル frame と同じ
/// `sub_session_id`（run の相関キー）を運ぶ。depth 0（親ターン）は field ごと省く。
#[test]
fn tool_activity_carries_the_run_session_key_for_nested_runs() {
    let nested = br#"{"m":"tool_activity","session":"room","activity_id":"act-1","call_id":"call-1","tool":"execute_shell","status":"started","depth":1,"sub_session_id":"subtask-s1","started_at":"2026-10-20T00:00:00Z","args":{}}"#;
    match parse_frame_bytes(nested).unwrap() {
        CoreMsg::ToolActivity(a) => {
            assert_eq!(a.depth, 1);
            assert_eq!(a.sub_session_id.as_deref(), Some("subtask-s1"));
        }
        other => panic!("{other:?}"),
    }
    let parent = br#"{"m":"tool_activity","session":"room","activity_id":"act-1","call_id":"call-1","tool":"execute_shell","status":"started","depth":0,"started_at":"2026-10-20T00:00:00Z","args":{}}"#;
    match parse_frame_bytes(parent).unwrap() {
        CoreMsg::ToolActivity(a) => assert_eq!(a.sub_session_id, None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn subtask_lifecycle_frame_parses_optional_fields() {
    let raw = br#"{"m":"subtask_lifecycle","session":"room","run_id":"run-1","status":"progress","detail":"half"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::SubtaskLifecycle(l) => {
            assert_eq!(l.run_id, "run-1");
            assert_eq!(l.status, "progress");
            assert_eq!(l.detail.as_deref(), Some("half"));
            assert_eq!(l.label, None);
            assert_eq!(l.duration_ms, None);
            // 走行の相関キー。frame が省いたら None（consumer は run id で代替できる）。
            assert_eq!(l.sub_session_id, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn subtask_lifecycle_frame_carries_the_run_session_key() {
    let raw = br#"{"m":"subtask_lifecycle","session":"room","run_id":"run-1","sub_session_id":"subtask-s1","status":"started","task":"do it"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::SubtaskLifecycle(l) => {
            assert_eq!(l.sub_session_id.as_deref(), Some("subtask-s1"));
            assert_eq!(l.task.as_deref(), Some("do it"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn tool_activity_without_a_binding_is_bad_request() {
    let raw = br#"{"m":"tool_activity","activity_id":"act-1"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
        other => panic!("{other:?}"),
    }
}

// 後方互換: 未知 `m` かつ id 無しの core→gate 通知（turn_failed を知らない旧 gateway 相当）は
// Unknown に落ち、handle_msg で write 0・keep（close しない）。DESIGN-EXTGATE-V3 §「RUNNING の
// 未知 m は unknown_message・keep」。これが崩れると外部 DI gateway を壊すので固定する。
#[test]
fn unknown_noid_notification_is_ignorable_unknown() {
    let raw = br#"{"m":"turn_failed_v99","session":"room","origin":"x"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Unknown { id, m } => {
            assert_eq!(id, None);
            assert_eq!(m, "turn_failed_v99");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn invoke_ok_frame_carries_result() {
    assert_eq!(
        invoke_ok_frame("call-1", &json!({"ok":true})),
        json!({"id":"call-1","m":"ok","result":{"ok":true}})
    );
    // JSON null は合法な result。
    assert_eq!(
        invoke_ok_frame("call-1", &Value::Null),
        json!({"id":"call-1","m":"ok","result":null})
    );
}

#[test]
fn hello_always_carries_versioned_runtime_capabilities_and_operations() {
    // Capability-zero clients still declare an explicit empty operation set in protocol v3.
    let plain = hello_frame_with_operations("h", "iid", "agent", None);
    assert_eq!(plain["protocol"], 3);
    assert_eq!(plain["operation_protocol"], 1);
    assert_eq!(plain["final_delivery"], "automatic");
    assert_eq!(plain["operations"], json!([]));
    // Some carries the declared operation set.
    let ops = json!([{"name":"reply"}]);
    let withops = hello_frame_with_operations("h", "iid", "agent", Some(&ops));
    assert_eq!(withops["operations"], ops);
}

#[test]
fn command_replies_preserve_result_and_old_core_unknown_message() {
    match parse_frame_bytes(br#"{"m":"ok","id":"cmd-1","result":{"models":[]}}"#).unwrap() {
        CoreMsg::Response(response) => {
            assert!(response.ok);
            assert_eq!(response.result, Some(json!({"models": []})));
        }
        other => panic!("{other:?}"),
    }
    match parse_frame_bytes(br#"{"m":"err","id":"cmd-2","code":"unknown_message","detail":null}"#)
        .unwrap()
    {
        CoreMsg::Response(response) => {
            assert!(!response.ok);
            assert_eq!(response.code.as_deref(), Some("unknown_message"));
            assert_eq!(response.message, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn command_frame_round_trips_all_contract_fields() {
    let session_key = "room-1";
    let expected = json!({
        "m": "command",
        "id": "cmd-set-1",
        "session": session_key,
        "caller": {"role": "owner"},
        "name": "set_model",
        "args": {"model": "openai:gpt-5"},
    });

    let outbound = command_frame(
        "cmd-set-1",
        session_key,
        &SaidCaller::Owner,
        "set_model",
        &json!({"model": "openai:gpt-5"}),
    );
    let encoded = serde_json::to_vec(&outbound).expect("command frame serializes");
    let round_tripped: Value =
        serde_json::from_slice(&encoded).expect("serialized command frame parses");

    assert_eq!(round_tripped, expected);
}

/// D-1085: `agent_prelude` is emitted only when set.
#[test]
fn said_agent_prelude_is_sent_only_when_set() {
    let mut context = SaidContext {
        caller: SaidCaller::Owner,
        start_turn: true,
        system_context: None,
        reply_target: None,
        reference: None,
        live_inbound_scope: LiveInboundScope::All,
        model_profile: TurnModelProfile::Realtime,
        agent_prelude: None,
        provisional_id: None,
    };
    let frame = |context: &SaidContext| {
        said_frame_with_context("said:1", "b", "o", "a", None, Some(context), "hi", &[])
    };
    assert!(frame(&context).get("agent_prelude").is_none());
    context.agent_prelude = Some("うん、".to_string());
    assert_eq!(frame(&context)["agent_prelude"], "うん、");
}

/// D-1091: a gateway that declares no environment must send byte-for-byte the frame it sent
/// before `run_env` existed. A gateway on the old release and a core on the new one therefore
/// keep interoperating, and a new gateway's hello is unchanged unless it declares something.
#[test]
fn an_empty_run_env_sends_exactly_the_previous_hello_frame() {
    use std::collections::BTreeMap;

    let ops = json!([{"name": "say"}]);
    for operations in [None, Some(&ops)] {
        let previous = hello_frame_with_operations("h", "iid", "agent", operations);
        let with_empty_env = crate::wire::hello_env::hello_frame_with_capabilities_and_env(
            "h",
            "iid",
            "agent",
            operations,
            crate::wire::RuntimeCapabilities::default(),
            &BTreeMap::new(),
        );
        assert_eq!(
            with_empty_env, previous,
            "the frame object must be identical"
        );
        assert_eq!(
            serde_json::to_vec(&with_empty_env).unwrap(),
            serde_json::to_vec(&previous).unwrap(),
            "the serialized bytes must be identical"
        );
        assert!(
            with_empty_env.get("run_env").is_none(),
            "an empty declaration must not add the field at all: {with_empty_env}"
        );
    }
}
