use anyhow::Result;
use async_trait::async_trait;

use crate::message::{IncomingMessage, OutgoingMessage};
use crate::traits::Gateway;

/// WebSocketゲートウェイ（スタブ）
///
/// WebSocketによるリアルタイム双方向通信を提供する。
/// 現在は未実装のプレースホルダー。
pub struct WebSocketGateway {
    _port: u16,
}

impl WebSocketGateway {
    pub fn new(port: u16) -> Self {
        Self { _port: port }
    }
}

#[async_trait]
impl Gateway for WebSocketGateway {
    fn name(&self) -> &str {
        "websocket"
    }

    async fn receive(&mut self) -> Result<IncomingMessage> {
        todo!("WebSocket gateway receive not yet implemented")
    }

    async fn send(&self, _message: OutgoingMessage) -> Result<()> {
        todo!("WebSocket gateway send not yet implemented")
    }

    async fn connect(&mut self) -> Result<()> {
        todo!("WebSocket gateway connect not yet implemented")
    }

    async fn disconnect(&mut self) -> Result<()> {
        todo!("WebSocket gateway disconnect not yet implemented")
    }
}
