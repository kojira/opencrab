    use super::*;

    #[test]
    fn s3_invoke_snapshot_rejects_stale_undeclared_and_mismatched_metadata() {
        let operations = serde_json::json!([{
            "name": "quasar.synthetic-v7",
            "description": "synthetic",
            "input_schema": {"type": "object"},
            "output_schema": null,
            "callback_schema": null,
            "authorization": {"allowed_callers": ["owner"]},
            "dispatch": "background",
            "sub_engine": "not_exposed",
            "sharing": "agent_bound",
            "effect": "state_change"
        }]);
        let capabilities = crate::wire::RuntimeCapabilities {
            final_delivery: crate::wire::FinalDelivery::Automatic,
        };
        let client = InstanceClient::blank(
            "instance".into(),
            "agent".into(),
            "author".into(),
            SayPolicy::AcceptToLiveQueue,
            Some(operations.clone()),
            capabilities,
            None,
        );
        let digest = crate::wire::runtime_declaration_digest_from_value(
            Some(&operations),
            capabilities,
        )
        .unwrap();
        assert_eq!(
            digest,
            "ae3998f8b460f71a2555ee1760ff82408162e4444a3a1d5dc5decac5020b889f"
        );
        let valid = Invoke {
            id: "call".into(),
            session_key: "address".into(),
            declaration_digest: digest,
            operation: "quasar.synthetic-v7".into(),
            dispatch: "background".into(),
            effect: "state_change".into(),
            continuation_id: None,
            payload: serde_json::json!({}),
        };
        assert_eq!(client.validate_invocation(&valid), Ok(()));

        let mut stale = valid.clone();
        stale.declaration_digest = "0".repeat(64);
        assert_eq!(client.validate_invocation(&stale), Err("operation_rejected"));
        let mut undeclared = valid.clone();
        undeclared.operation = "other".into();
        assert_eq!(client.validate_invocation(&undeclared), Err("operation_unknown"));
        let mut metadata = valid.clone();
        metadata.dispatch = "inline".into();
        assert_eq!(client.validate_invocation(&metadata), Err("operation_rejected"));
    }

    #[tokio::test]
    async fn malformed_present_reply_target_never_reaches_live_delivery() {
        for payload in [
            serde_json::json!({"text": "reply", "reply_target": null}),
            serde_json::json!({"text": "reply", "reply_target": 42}),
            serde_json::json!({"text": "reply", "reply_target": ""}),
        ] {
            let client = InstanceClient::blank(
                "instance".into(),
                "agent".into(),
                "author".into(),
                SayPolicy::AcceptToLiveQueue,
                None,
                crate::wire::RuntimeCapabilities::default(),
                None,
            );
            {
                let mut inner = client.inner.lock().await;
                inner.closed = false;
                inner.pending_turn.insert(
                    "binding".into(),
                    PendingTurn {
                        saw_utterance: false,
                        reply_origin: ReplyOrigin::Single("pending-origin".into()),
                    },
                );
            }

            let closed = handle_say(
                &client,
                Say {
                    id: "say:malformed".into(),
                    session_key: "address".into(),
                    payload,
                },
                0,
            )
            .await;

            assert!(!closed);
            let inner = client.inner.lock().await;
            assert!(
                inner
                    .live
                    .get("address")
                    .is_none_or(|queue| queue.events.is_empty()),
                "malformed reply_target reached the live delivery queue"
            );
            assert!(matches!(
                inner.pending_turn.get("binding"),
                Some(PendingTurn {
                    saw_utterance: false,
                    ..
                })
            ));
        }
    }

    /// D-1093: a say without `reply_target` is a plain message. The client must not fill a
    /// reply destination from the turn's triggering said.
    #[tokio::test]
    async fn plain_say_gets_no_reply_origin_from_the_pending_turn() {
        let client = InstanceClient::blank(
            "instance".into(),
            "agent".into(),
            "author".into(),
            SayPolicy::AcceptToLiveQueue,
            None,
            crate::wire::RuntimeCapabilities::default(),
            None,
        );
        {
            let mut inner = client.inner.lock().await;
            inner.closed = false;
            inner.pending_turn.insert(
                "binding".into(),
                PendingTurn {
                    saw_utterance: false,
                    reply_origin: ReplyOrigin::Single("pending-origin".into()),
                },
            );
        }
        // The blank client has no transport, so acknowledging the say closes it afterwards; the
        // queued live event is what this test checks.
        let _ = handle_say(
            &client,
            Say {
                id: "say:plain".into(),
                session_key: "address".into(),
                payload: serde_json::json!({"text": "plain"}),
            },
            0,
        )
        .await;
        let inner = client.inner.lock().await;
        let events = &inner.live.get("address").expect("queued").events;
        let message = events
            .iter()
            .find(|event| matches!(event, LiveEvent::Message { .. }))
            .expect("say queued");
        assert!(
            matches!(message, LiveEvent::Message { reply_origin: None, text, .. } if text == "plain"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn completed_target_and_completed_no_reply_are_exclusive() {
        let client = InstanceClient::blank(
            "instance".into(),
            "agent".into(),
            "author".into(),
            SayPolicy::AcceptToLiveQueue,
            None,
            crate::wire::RuntimeCapabilities::default(),
            None,
        );
        {
            let mut inner = client.inner.lock().await;
            inner.closed = false;
        }
        handle_activity(
            &client,
            Activity {
                session_key: "address".into(),
                activity_id: "activity".into(),
                state: "started".into(),
                origin: None,
                completed_target: None,
                silent_origins: None,
            },
            0,
        )
        .await;
        handle_activity(
            &client,
            Activity {
                session_key: "address".into(),
                activity_id: "activity".into(),
                state: "ended".into(),
                origin: None,
                completed_target: Some("utterance".into()),
                silent_origins: None,
            },
            0,
        )
        .await;

        assert!(matches!(
            client.next_live("address").await,
            Some(LiveEvent::Activity { state, .. }) if state == "started"
        ));
        assert!(matches!(
            client.next_live("address").await,
            Some(LiveEvent::Activity { state, .. }) if state == "ended"
        ));
        assert_eq!(
            client.next_live("address").await,
            Some(LiveEvent::Completed {
                target: "utterance".into()
            })
        );
    }

#[tokio::test]
async fn command_timeout_removes_pending_and_ignores_late_reply() {
    let client = InstanceClient::blank(
        "instance".to_string(),
        "agent".to_string(),
        "author".to_string(),
        SayPolicy::AcceptToLiveQueue,
        None,
        crate::wire::RuntimeCapabilities::default(),
        None,
    );
    let (write_tx, mut write_rx) = mpsc::unbounded_channel();
    client.write.lock().await.tx = write_tx;
    {
        let mut inner = client.inner.lock().await;
        inner.closed = false;
    }

    let outcome = client
        .command(
            "address",
            &SaidCaller::Owner,
            "list_models",
            &serde_json::json!({}),
            Duration::from_millis(1),
        )
        .await;
    assert_eq!(outcome, Err(CommandError::Timeout));
    let sent = write_rx.recv().await.unwrap();
    let id = sent["id"].as_str().unwrap().to_string();
    assert!(client.inner.lock().await.pending_commands.is_empty());

    handle_response(
        &client,
        WireResponse {
            id,
            ok: true,
            seq: None,
            result: Some(serde_json::json!({})),
            session_id: None,
            code: None,
            detail: None,
            message: None,
        },
        0,
    )
    .await;
    assert!(!client.inner.lock().await.closed);
    assert!(write_rx.try_recv().is_err(), "late reply must not trigger a retry");
}
