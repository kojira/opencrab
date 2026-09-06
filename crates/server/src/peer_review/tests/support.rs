    use super::*;
    use async_trait::async_trait;
    use opencrab_db::queries::TrustedUserPermission;
    use opencrab_gateway::GatewayCaller;
    use std::sync::Mutex;

    /// Discord の配送口と同じ規約（数値宛先 / `<@id>` / 1900 chars）を持つフェイク。
    /// 送信を記録するだけで、`fail_at` を指定すると N 通目で失敗する。
    struct FakeDelivery {
        sent: Mutex<Vec<(String, String)>>,
        /// 0-origin の添字。この通数目の送信で失敗させる。
        fail_at: Option<usize>,
    }

    impl FakeDelivery {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail_at: None,
            }
        }
        fn failing_at(i: usize) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail_at: Some(i),
            }
        }
        fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl TextDelivery for FakeDelivery {
        fn validate_target(&self, target: &str) -> Result<(), String> {
            if target.parse::<u64>().is_ok() {
                Ok(())
            } else {
                Err(format!("無効なchannel_id: {target}"))
            }
        }
        fn mention(&self, user_id: &str) -> String {
            format!("<@{user_id}>")
        }
        fn chunk_limit(&self) -> usize {
            1900
        }
        async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
            if self.fail_at == Some(self.count()) {
                return Err("transport down".to_string());
            }
            self.sent
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string()));
            Ok(())
        }
    }

    const CHUNK_LIMIT: usize = 1900;

    fn ctx_with_session() -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-a").with_session_id("sess-1")
    }

