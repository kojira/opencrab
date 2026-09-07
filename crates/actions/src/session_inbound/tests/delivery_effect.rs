fn er(response: &str) -> EngineResult {
    EngineResult {
        response: response.into(),
        iterations: 1,
        tool_calls_made: 2,
        stopped_by_limit: false,
        last_posting_utterance_id: None,
        last_generation_had_continuation_speech: false,
        xml_fallback_parses: 0,
    }
}

#[test]
fn delivery_effect_maps_engine_result() {
    let ctx = crate::no_reply::DeliveryContext::default();
    assert_eq!(
        delivery_effect(Ok(er("hello")), ctx),
        DeliveryEffect::Text {
            body: "hello".into(),
            stopped_by_limit: false,
            tool_calls_made: 2,
            iterations: 1,
        }
    );
    assert_eq!(delivery_effect(Ok(er("NO_REPLY")), ctx), DeliveryEffect::NoReply);
    let empty = EngineResult {
        response: String::new(),
        iterations: 0,
        tool_calls_made: 0,
        stopped_by_limit: false,
        last_posting_utterance_id: None,
        last_generation_had_continuation_speech: false,
        xml_fallback_parses: 0,
    };
    assert_eq!(delivery_effect(Ok(empty), ctx), DeliveryEffect::Empty);
    match delivery_effect(Err(anyhow::anyhow!("boom")), ctx) {
        DeliveryEffect::Failed { error } => assert!(error.contains("boom"), "{error}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn delivery_effect_terminates_at_no_reply() {
    let ctx = crate::no_reply::DeliveryContext::default();
    match delivery_effect(Ok(er("本文だけ話す NO_REPLY これはゴミ")), ctx) {
        DeliveryEffect::Text { body, .. } => {
            assert_eq!(body, "本文だけ話す");
            assert!(!body.contains("NO_REPLY"), "body に NO_REPLY 混入: {body}");
            assert!(!body.contains("ゴミ"), "body に破棄テキスト混入: {body}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(
        delivery_effect(Ok(er("NO_REPLY 続くゴミ")), ctx),
        DeliveryEffect::NoReply
    );
}

#[test]
fn delivery_effect_strips_tail_continue_marker() {
    let ctx = crate::no_reply::DeliveryContext::default();
    match delivery_effect(Ok(er("確認して返すね⚡\nCONTINUE")), ctx) {
        DeliveryEffect::Text { body, .. } => {
            assert_eq!(body, "確認して返すね⚡");
            assert!(!body.contains("CONTINUE"), "body に CONTINUE 混入: {body}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    match delivery_effect(Ok(er("まず CONTINUE を確認します")), ctx) {
        DeliveryEffect::Text { body, .. } => assert_eq!(body, "まず CONTINUE を確認します"),
        other => panic!("expected Text, got {other:?}"),
    }
}
