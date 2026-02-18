use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::message::{
    IncomingMessage, MessageContent, MessageSource, OutgoingMessage, Sender,
};
use crate::traits::Gateway;

/// REST APIゲートウェイ
///
/// Axum HTTPサーバーとCoreエンジンの間でメッセージを仲介する。
/// mpscチャンネルで受信メッセージを受け取り、oneshotチャンネルで
/// 個別リクエストへのレスポンスを返す。
///
/// # 使い方
///
/// ```ignore
/// let gateway = RestGateway::new(32);
///
/// // HTTPハンドラ側
/// let msg_id = gateway.submit_message(incoming).await?;
/// let response = gateway.wait_response(&msg_id).await?;
///
/// // Core側
/// let incoming = gateway.receive().await?;
/// gateway.send(outgoing).await?;
/// ```
pub struct RestGateway {
    tx_in: mpsc::Sender<IncomingMessage>,
    rx_in: Mutex<mpsc::Receiver<IncomingMessage>>,
    pending_responses: Arc<Mutex<HashMap<String, oneshot::Sender<OutgoingMessage>>>>,
}

impl RestGateway {
    /// 新しいRestGatewayを作成する
    ///
    /// `buffer_size` は受信メッセージキューの容量を指定する。
    pub fn new(buffer_size: usize) -> Self {
        let (tx_in, rx_in) = mpsc::channel(buffer_size);
        Self {
            tx_in,
            rx_in: Mutex::new(rx_in),
            pending_responses: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// HTTPハンドラからメッセージを投入する
    ///
    /// メッセージをキューに入れ、レスポンス用のoneshotチャンネルを登録する。
    /// 返されるメッセージIDを使って `wait_response()` でレスポンスを待つ。
    pub async fn submit_message(&self, message: IncomingMessage) -> Result<String> {
        let message_id = message.id.clone();

        let (resp_tx, _resp_rx) = oneshot::channel();
        {
            let mut pending = self.pending_responses.lock().await;
            pending.insert(message_id.clone(), resp_tx);
        }

        self.tx_in
            .send(message)
            .await
            .context("Failed to submit message to gateway channel")?;

        debug!(message_id = %message_id, "Message submitted to REST gateway");
        Ok(message_id)
    }

    /// HTTPハンドラからメッセージを投入し、レスポンスを待つ
    ///
    /// `submit_message` と `wait_response` を一度に行うユーティリティ。
    pub async fn submit_and_wait(&self, message: IncomingMessage) -> Result<OutgoingMessage> {
        let message_id = message.id.clone();

        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut pending = self.pending_responses.lock().await;
            pending.insert(message_id.clone(), resp_tx);
        }

        self.tx_in
            .send(message)
            .await
            .context("Failed to submit message to gateway channel")?;

        debug!(message_id = %message_id, "Message submitted, waiting for response");

        resp_rx
            .await
            .context("Response channel closed before receiving response")
    }

    /// 指定メッセージIDのレスポンスを待つ
    ///
    /// 対応する `submit_message` で登録されたoneshotチャンネルから
    /// レスポンスを受信する。このメソッドは新たにoneshotレシーバーを
    /// 作成し直すため、`submit_and_wait` の使用を推奨する。
    pub async fn wait_response(&self, message_id: &str) -> Result<OutgoingMessage> {
        // NOTE: submit_and_waitの利用を推奨。
        // このメソッドはsubmit_messageで既にoneshotが登録されている前提で、
        // 新しいoneshotに差し替える方式で動作する。
        let (resp_tx, resp_rx) = oneshot::channel();
        {
            let mut pending = self.pending_responses.lock().await;
            // 古いsenderを取り除いて新しいものに差し替え
            pending.remove(message_id);
            pending.insert(message_id.to_string(), resp_tx);
        }

        resp_rx
            .await
            .context("Response channel closed before receiving response")
    }

    /// テキストメッセージを簡単に投入するヘルパー
    pub async fn submit_text(
        &self,
        text: impl Into<String>,
        sender_id: impl Into<String>,
        sender_name: impl Into<String>,
    ) -> Result<String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let message = IncomingMessage::new(
            MessageSource::Rest {
                request_id: request_id.clone(),
            },
            MessageContent::text(text),
            Sender::user(sender_id, sender_name),
        );
        self.submit_message(message).await
    }
}

#[async_trait]
impl Gateway for RestGateway {
    fn name(&self) -> &str {
        "rest"
    }

    async fn receive(&mut self) -> Result<IncomingMessage> {
        let mut rx = self.rx_in.lock().await;
        rx.recv()
            .await
            .context("REST gateway receive channel closed")
    }

    async fn send(&self, message: OutgoingMessage) -> Result<()> {
        let reply_to = match &message.reply_to {
            Some(id) => id.clone(),
            None => {
                warn!("REST gateway send() called without reply_to; message dropped");
                return Ok(());
            }
        };

        let mut pending = self.pending_responses.lock().await;
        if let Some(sender) = pending.remove(&reply_to) {
            if sender.send(message).is_err() {
                warn!(
                    reply_to = %reply_to,
                    "Failed to send response: receiver already dropped"
                );
            } else {
                debug!(reply_to = %reply_to, "Response sent via REST gateway");
            }
        } else {
            warn!(
                reply_to = %reply_to,
                "No pending response found for message"
            );
        }

        Ok(())
    }

    /// REST Gatewayではconnectはno-op（Axumサーバーが別途HTTPを処理する）
    async fn connect(&mut self) -> Result<()> {
        debug!("REST gateway connected (no-op)");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        debug!("REST gateway disconnected");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::message::*;
    use crate::traits::Gateway;

    use super::*;

    #[tokio::test]
    async fn test_submit_and_receive() {
        let mut gateway = RestGateway::new(32);
        let msg = IncomingMessage::new(
            MessageSource::Rest {
                request_id: "req-1".to_string(),
            },
            MessageContent::text("test message"),
            Sender::user("user-1", "User One"),
        );
        let msg_id = gateway.submit_message(msg).await.unwrap();
        assert!(!msg_id.is_empty());

        let received = gateway.receive().await.unwrap();
        assert_eq!(received.content.as_text(), Some("test message"));
    }

    #[tokio::test]
    async fn test_submit_and_wait() {
        let gateway = RestGateway::new(32);
        let msg = IncomingMessage::new(
            MessageSource::Rest {
                request_id: "req-1".to_string(),
            },
            MessageContent::text("request text"),
            Sender::user("user-1", "User One"),
        );
        let msg_id = msg.id.clone();

        // Spawn a task that sends the response through the pending oneshot
        let pending = Arc::clone(&gateway.pending_responses);
        let handle = tokio::spawn(async move {
            // Brief yield to let submit_and_wait register the oneshot
            tokio::task::yield_now().await;
            let mut map = pending.lock().await;
            if let Some(sender) = map.remove(&msg_id) {
                let reply = OutgoingMessage::text_reply("response text", &msg_id);
                sender.send(reply).ok();
            }
        });

        let response = gateway.submit_and_wait(msg).await.unwrap();
        assert_eq!(response.content.as_text(), Some("response text"));

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_without_reply_to() {
        let gateway = RestGateway::new(32);
        let msg = OutgoingMessage {
            content: MessageContent::text("orphan message"),
            target: MessageTarget::Broadcast,
            reply_to: None,
            metadata: std::collections::HashMap::new(),
        };
        // Should not error; message is simply dropped
        let result = gateway.send(msg).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_submit_text_helper() {
        let mut gateway = RestGateway::new(32);
        let msg_id = gateway.submit_text("hello", "user-1", "User").await.unwrap();
        assert!(!msg_id.is_empty());

        let received = gateway.receive().await.unwrap();
        assert_eq!(received.content.as_text(), Some("hello"));
        assert_eq!(received.sender.id, "user-1");
        assert_eq!(received.sender.name, "User");
    }
}
