use super::*;

#[test]
fn config_bytes_are_compact_author_id() {
    let bytes = config_bytes("owner-1");
    assert_eq!(bytes, br#"{"author_id":"owner-1"}"#);
}

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

#[test]
fn config_digest_is_sha256_lowerhex() {
    let d = config_digest("owner-1");
    assert_eq!(d.len(), 64);
    assert!(d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    assert_eq!(d, config_digest("owner-1"));
    assert_ne!(d, config_digest("owner-2"));
}

#[test]
fn parse_bind_ok() {
    let raw = br#"{"id":"bind:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"bind","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","address":"web-a-c"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Bind(b) => {
            assert_eq!(b.address, "web-a-c");
            assert_eq!(b.binding_id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        }
        other => panic!("{other:?}"),
    }
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
            "binding_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
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
    let raw = br#"{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"invoke","binding_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","operation":"reply","context":{"continuation_id":null},"payload":{"event":"e7","text":"hi"}}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Invoke(i) => {
            assert_eq!(i.id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
            assert_eq!(i.binding_id, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
            assert_eq!(i.operation, "reply");
            assert_eq!(i.continuation_id, None);
            assert_eq!(i.payload, json!({"event":"e7","text":"hi"}));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_invoke_missing_payload_is_bad_request() {
    let raw = br#"{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"invoke","binding_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","operation":"reply","context":{"continuation_id":null}}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
        other => panic!("{other:?}"),
    }
}

// #964: read は次の request に新しく含める投稿の origin を運ぶ。
#[test]
fn parse_activity_read_carries_origin() {
    let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"read","origin":"omo-1"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Activity(a) => {
            assert_eq!(a.state, "read");
            assert_eq!(a.origin.as_deref(), Some("omo-1"));
            assert_eq!(a.completed_target, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_activity_stopped_is_a_non_final_typing_boundary() {
    let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"stopped"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::Activity(a) => {
            assert_eq!(a.state, "stopped");
            assert_eq!(a.origin, None);
            assert_eq!(a.silent_origins, None);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// origin 欠落は None（後方互換）。additive の未知 field も無視。
#[test]
fn parse_activity_without_origin_is_none() {
    let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"ended","future_field":42}"#;
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
    let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"ended","completed_target":"cccccccc-cccc-4ccc-8ccc-cccccccccccc"}"#;
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
            "{{\"m\":\"activity\",\"binding_id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"activity_id\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"state\":\"ended\"{field}}}"
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
            "{{\"m\":\"activity\",\"binding_id\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",\"activity_id\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\",\"state\":\"ended\",\"silent_origins\":{value}}}"
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

// R3(❌): turn_failed は binding_id + origin を運ぶ。error 本文（未知 field）は無視。
#[test]
fn parse_turn_failed_ok() {
    let raw = br#"{"m":"turn_failed","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","origin":"boom-1","error":"leaked-should-be-ignored"}"#;
    match parse_frame_bytes(raw).unwrap() {
        CoreMsg::TurnFailed(t) => {
            assert_eq!(t.binding_id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
            assert_eq!(t.origin, "boom-1");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_turn_failed_missing_origin_is_bad_request() {
    let raw = br#"{"m":"turn_failed","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}"#;
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
    let raw = br#"{"m":"turn_failed_v99","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","origin":"x"}"#;
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
fn hello_with_operations_optional() {
    // None は従来の hello（operations field なし＝能力ゼロ）。
    let plain = hello_frame_with_operations("h", "iid", 1, &"a".repeat(64), None);
    assert!(plain.get("operations").is_none());
    // Some は operations を載せる。
    let ops = json!([{"name":"reply"}]);
    let withops = hello_frame_with_operations("h", "iid", 1, &"a".repeat(64), Some(&ops));
    assert_eq!(withops["operations"], ops);
}
