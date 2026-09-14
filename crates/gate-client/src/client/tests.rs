    use super::*;

    #[tokio::test]
    async fn malformed_present_reply_target_never_reaches_live_delivery() {
        for payload in [
            serde_json::json!({"text": "reply", "reply_target": null}),
            serde_json::json!({"text": "reply", "reply_target": 42}),
            serde_json::json!({"text": "reply", "reply_target": ""}),
        ] {
            let client = InstanceClient::blank(
                "instance".into(),
                "author".into(),
                SayPolicy::AcceptToLiveQueue,
                None,
                None,
            );
            {
                let mut inner = client.inner.lock().await;
                inner.closed = false;
                inner
                    .acknowledged
                    .insert("address".into(), "binding".into());
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
                    binding_id: "binding".into(),
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

    #[tokio::test]
    async fn completed_target_and_completed_no_reply_are_exclusive() {
        let client = InstanceClient::blank(
            "instance".into(),
            "author".into(),
            SayPolicy::AcceptToLiveQueue,
            None,
            None,
        );
        {
            let mut inner = client.inner.lock().await;
            inner.closed = false;
            inner
                .acknowledged
                .insert("address".into(), "binding".into());
        }
        handle_activity(
            &client,
            Activity {
                binding_id: "binding".into(),
                activity_id: "activity".into(),
                state: "started".into(),
                origin: None,
                completed_target: None,
            },
        )
        .await;
        handle_activity(
            &client,
            Activity {
                binding_id: "binding".into(),
                activity_id: "activity".into(),
                state: "ended".into(),
                origin: None,
                completed_target: Some("utterance".into()),
            },
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
