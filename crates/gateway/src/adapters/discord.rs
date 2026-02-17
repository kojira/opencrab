use anyhow::Result;
use async_trait::async_trait;

use crate::message::{IncomingMessage, OutgoingMessage};
use crate::traits::Gateway;

/// Discordゲートウェイ（スタブ）
///
/// Discord Botとしてメッセージの送受信を行う。
/// 現在は未実装のプレースホルダー。
pub struct DiscordGateway {
    _token: String,
}

impl DiscordGateway {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            _token: token.into(),
        }
    }
}

#[async_trait]
impl Gateway for DiscordGateway {
    fn name(&self) -> &str {
        "discord"
    }

    async fn receive(&mut self) -> Result<IncomingMessage> {
        todo!("Discord gateway receive not yet implemented")
    }

    async fn send(&self, _message: OutgoingMessage) -> Result<()> {
        todo!("Discord gateway send not yet implemented")
    }

    async fn connect(&mut self) -> Result<()> {
        todo!("Discord gateway connect not yet implemented")
    }

    async fn disconnect(&mut self) -> Result<()> {
        todo!("Discord gateway disconnect not yet implemented")
    }
}
