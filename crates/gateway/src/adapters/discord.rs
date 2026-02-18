use std::sync::Arc;

use anyhow::{Context as AnyhowContext, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use serenity::all::{
    ChannelId, Client, Context, EventHandler, GatewayIntents,
    Message as SerenityMessage, Ready,
};
use serenity::http::Http;

use crate::message::{
    Channel, IncomingMessage, MessageContent, MessageSource, MessageTarget,
    OutgoingMessage, Sender,
};
use crate::traits::Gateway;

/// Discordゲートウェイ
///
/// serenityクレートを使用してDiscord Botとしてメッセージの送受信を行う。
/// Cargo feature `discord` を有効にすることで利用可能になる。
///
/// # プラグイン分離
///
/// このモジュールは `#[cfg(feature = "discord")]` で条件付きコンパイルされる。
/// `discord` featureを有効にしない限り、serenityクレートは依存関係に含まれず、
/// 本体のビルドに一切影響しない。
///
/// # 使い方
///
/// ```ignore
/// let gateway = DiscordGateway::new("your-bot-token");
/// gateway.start().await?;
///
/// // メッセージ受信（ブロッキング）
/// let msg = gateway.recv().await?;
///
/// // チャンネルにテキスト送信
/// gateway.send_to_channel(channel_id, "Hello!").await?;
/// ```
pub struct DiscordGateway {
    token: String,
    rx: Mutex<mpsc::Receiver<IncomingMessage>>,
    tx: mpsc::Sender<IncomingMessage>,
    http: Arc<Http>,
    shard_manager: Mutex<Option<Arc<serenity::gateway::ShardManager>>>,
}

impl DiscordGateway {
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();
        let (tx, rx) = mpsc::channel(256);
        let http = Arc::new(Http::new(&token));
        Self {
            token,
            rx: Mutex::new(rx),
            tx,
            http,
            shard_manager: Mutex::new(None),
        }
    }

    /// Bot接続を開始する（バックグラウンドタスクとして起動）
    pub async fn start(&self) -> Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let handler = DiscordHandler {
            tx: self.tx.clone(),
        };

        let mut client = Client::builder(&self.token, intents)
            .event_handler(handler)
            .await
            .context("Failed to create Discord client")?;

        let shard_manager = client.shard_manager.clone();
        {
            let mut sm = self.shard_manager.lock().await;
            *sm = Some(shard_manager);
        }

        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                error!("Discord client error: {e}");
            }
        });

        info!("Discord gateway starting...");
        Ok(())
    }

    /// メッセージを受信する（ブロッキング）
    ///
    /// Discordからメッセージが届くまで待機する。
    /// 受信ループから呼ぶことを想定。
    pub async fn recv(&self) -> Result<IncomingMessage> {
        let mut rx = self.rx.lock().await;
        rx.recv().await.context("Discord gateway channel closed")
    }

    /// 指定チャンネルにテキストメッセージを送信する
    pub async fn send_to_channel(&self, channel_id: u64, text: &str) -> Result<()> {
        // Discord APIの文字数制限（2000文字）
        if text.len() <= 2000 {
            ChannelId::new(channel_id)
                .say(&self.http, text)
                .await
                .context("Failed to send message to Discord channel")?;
        } else {
            // 長いメッセージは分割送信
            for chunk in split_message(text, 2000) {
                ChannelId::new(channel_id)
                    .say(&self.http, &chunk)
                    .await
                    .context("Failed to send message chunk to Discord channel")?;
            }
        }
        Ok(())
    }

    /// Botをシャットダウンする
    pub async fn shutdown(&self) {
        let sm = self.shard_manager.lock().await;
        if let Some(ref manager) = *sm {
            manager.shutdown_all().await;
            info!("Discord gateway shut down");
        }
    }
}

/// Discordの2000文字制限に合わせてメッセージを分割する
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        // 1行が制限を超える場合はさらに分割
        if line.len() > max_len {
            if !current.is_empty() {
                chunks.push(current.clone());
                current.clear();
            }
            for chunk in line.as_bytes().chunks(max_len) {
                chunks.push(String::from_utf8_lossy(chunk).to_string());
            }
            continue;
        }

        if current.len() + line.len() + 1 > max_len {
            chunks.push(current.clone());
            current.clear();
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ==================== Serenity Event Handler ====================

struct DiscordHandler {
    tx: mpsc::Sender<IncomingMessage>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, _ctx: Context, msg: SerenityMessage) {
        info!(
            author = %msg.author.name,
            bot = msg.author.bot,
            content = %msg.content.chars().take(50).collect::<String>(),
            "Discord message event received"
        );

        // Bot自身のメッセージは無視（無限ループ防止）
        if msg.author.bot {
            return;
        }

        let guild_id = msg
            .guild_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let channel_id = msg.channel_id.to_string();

        let incoming = IncomingMessage::new(
            MessageSource::Discord {
                guild_id,
                channel_id: channel_id.clone(),
            },
            MessageContent::text(&msg.content),
            Sender::user(msg.author.id.to_string(), &msg.author.name),
        )
        .with_channel(Channel {
            id: channel_id,
            name: msg.channel_id.to_string(),
        })
        .with_metadata(
            "discord_message_id",
            serde_json::json!(msg.id.to_string()),
        );

        if let Err(e) = self.tx.send(incoming).await {
            warn!("Failed to forward Discord message to gateway: {e}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(
            "Discord bot connected as {} (id: {})",
            ready.user.name, ready.user.id,
        );
    }
}

// ==================== Gateway Trait Implementation ====================

#[async_trait]
impl Gateway for DiscordGateway {
    fn name(&self) -> &str {
        "discord"
    }

    async fn receive(&mut self) -> Result<IncomingMessage> {
        self.recv().await
    }

    async fn send(&self, message: OutgoingMessage) -> Result<()> {
        let text = message
            .content
            .as_text()
            .unwrap_or("[unsupported content type]");

        let channel_id = match &message.target {
            MessageTarget::Channel { id } => id
                .parse::<u64>()
                .context("Invalid channel ID for Discord send")?,
            _ => {
                if let Some(ch) = message.metadata.get("discord_channel_id") {
                    ch.as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .context("Invalid discord_channel_id in metadata")?
                } else {
                    warn!("Discord send: no target channel specified, dropping message");
                    return Ok(());
                }
            }
        };

        self.send_to_channel(channel_id, text).await
    }

    async fn connect(&mut self) -> Result<()> {
        self.start().await
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_message_short() {
        let chunks = split_message("hello", 2000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_split_message_long() {
        let text = "a".repeat(2500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() <= 2000);
    }

    #[test]
    fn test_split_message_multiline() {
        let lines: Vec<String> = (0..100).map(|i| format!("Line {i}: some content here")).collect();
        let text = lines.join("\n");
        let chunks = split_message(&text, 200);
        for chunk in &chunks {
            assert!(chunk.len() <= 200);
        }
    }
}
