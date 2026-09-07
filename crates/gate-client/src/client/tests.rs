    use super::*;

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
