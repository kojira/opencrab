fn er(response: &str) -> EngineResult {
    EngineResult {
        response: response.into(),
        iterations: 1,
        tool_calls_made: 2,
        stopped_by_limit: false,
        explicit_termination: None,
        last_posting_utterance_id: None,
        last_generation_had_continuation_speech: false,
        xml_fallback_parses: 0,
    }
}

fn no_reply(response: &str) -> EngineResult {
    EngineResult {
        explicit_termination: Some(opencrab_core::ExplicitTermination::NoReply),
        ..er(response)
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
    assert_eq!(delivery_effect(Ok(no_reply("")), ctx), DeliveryEffect::NoReply);
    let empty = EngineResult {
        response: String::new(),
        iterations: 0,
        tool_calls_made: 0,
        stopped_by_limit: false,
        explicit_termination: None,
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
    match delivery_effect(Ok(no_reply("本文だけ話す")), ctx) {
        DeliveryEffect::Text { body, .. } => assert_eq!(body, "本文だけ話す"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(delivery_effect(Ok(no_reply("")), ctx), DeliveryEffect::NoReply);
}
