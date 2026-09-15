use super::*;

struct RecordingTransport {
    reactions: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl RecordingTransport {
    fn new() -> Self {
        Self {
            reactions: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl DiscordTransport for RecordingTransport {
    async fn create_message(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(serde_json::json!({}))
    }

    async fn reply_message(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(serde_json::json!({}))
    }

    async fn add_reaction(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(serde_json::json!({}))
    }

    async fn add_system_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome {
        self.reactions.lock().unwrap().push((
            channel_id.to_string(),
            message_id.to_string(),
            emoji.to_string(),
        ));
        TransportOutcome::Ok(serde_json::json!({}))
    }

    async fn get_message(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(serde_json::json!({}))
    }

    async fn get_user(&self, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(serde_json::json!({}))
    }

    async fn broadcast_typing(&self, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(serde_json::json!({}))
    }
}

#[tokio::test]
async fn only_b_gets_no_reply_and_completion_keeps_its_own_target() {
    let recording = Arc::new(RecordingTransport::new());
    let transport: Arc<dyn DiscordTransport> = recording.clone();

    // A has visible output, so no CompletedNoReply event reaches this boundary.
    react_no_reply(&transport, None, "🤐").await;
    react_no_reply(&transport, Some("discord:message:v1:20:200"), "🤐").await;
    react_system_on(&transport, "20", "300", "🏁").await;

    assert_eq!(
        recording.reactions.lock().unwrap().as_slice(),
        [
            ("20".to_string(), "200".to_string(), "🤐".to_string()),
            ("20".to_string(), "300".to_string(), "🏁".to_string()),
        ]
    );
}
