use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// メッセージソース（どのプラットフォームから来たか）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageSource {
    Rest {
        request_id: String,
    },
    WebSocket {
        connection_id: String,
    },
    Discord {
        guild_id: String,
        channel_id: String,
    },
    Cli {
        session_id: String,
    },
    Slack {
        workspace_id: String,
        channel_id: String,
    },
    Line {
        user_id: String,
    },
}

/// メッセージコンテンツ
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum MessageContent {
    Text(String),
    Image { url: String, alt: Option<String> },
    Multi(Vec<ContentPart>),
}

impl MessageContent {
    /// テキストコンテンツを簡単に作成
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    /// テキスト内容を取得（Textの場合のみ）
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// マルチパートコンテンツの各パーツ
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ContentPart {
    Text(String),
    Image { url: String, alt: Option<String> },
}

/// メッセージ送信者
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    pub name: String,
    pub is_bot: bool,
    pub avatar_url: Option<String>,
}

impl Sender {
    pub fn user(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            is_bot: false,
            avatar_url: None,
        }
    }

    pub fn bot(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            is_bot: true,
            avatar_url: None,
        }
    }

    pub fn with_avatar(mut self, url: impl Into<String>) -> Self {
        self.avatar_url = Some(url.into());
        self
    }
}

/// チャンネル情報
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
}

/// 受信メッセージ（外部プラットフォーム → Gateway → Core）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub id: String,
    pub source: MessageSource,
    pub content: MessageContent,
    pub sender: Sender,
    pub channel: Option<Channel>,
    pub timestamp: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl IncomingMessage {
    /// 新しい受信メッセージを作成
    pub fn new(
        source: MessageSource,
        content: MessageContent,
        sender: Sender,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            source,
            content,
            sender,
            channel: None,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        }
    }

    pub fn with_channel(mut self, channel: Channel) -> Self {
        self.channel = Some(channel);
        self
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// メッセージ送信先
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageTarget {
    Channel { id: String },
    DirectMessage { user_id: String },
    Broadcast,
}

/// 送信メッセージ（Core → Gateway → 外部プラットフォーム）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub content: MessageContent,
    pub target: MessageTarget,
    pub reply_to: Option<String>,
    pub metadata: HashMap<String, serde_json::Value>,
}

impl OutgoingMessage {
    /// テキスト返信を簡単に作成
    pub fn text_reply(text: impl Into<String>, reply_to: impl Into<String>) -> Self {
        Self {
            content: MessageContent::Text(text.into()),
            target: MessageTarget::Broadcast,
            reply_to: Some(reply_to.into()),
            metadata: HashMap::new(),
        }
    }

    /// チャンネルへのテキストメッセージを作成
    pub fn text_to_channel(text: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self {
            content: MessageContent::Text(text.into()),
            target: MessageTarget::Channel {
                id: channel_id.into(),
            },
            reply_to: None,
            metadata: HashMap::new(),
        }
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// ゲートウェイ設定
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub name: String,
    pub enabled: bool,
    pub settings: HashMap<String, serde_json::Value>,
}

impl GatewayConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            settings: HashMap::new(),
        }
    }

    pub fn with_setting(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.settings.insert(key.into(), value);
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_incoming_message_new() {
        let msg = IncomingMessage::new(
            MessageSource::Rest {
                request_id: "req-1".to_string(),
            },
            MessageContent::text("test message"),
            Sender::user("user-1", "User One"),
        );
        assert!(!msg.id.is_empty());
        assert_eq!(msg.content.as_text(), Some("test message"));
    }

    #[test]
    fn test_text_reply() {
        let reply = OutgoingMessage::text_reply("hi", "msg-1");
        assert_eq!(reply.reply_to, Some("msg-1".to_string()));
        assert_eq!(reply.content.as_text(), Some("hi"));
    }

    #[test]
    fn test_message_content_as_text() {
        assert_eq!(MessageContent::Text("hello".to_string()).as_text(), Some("hello"));
        let image = MessageContent::Image {
            url: "http://example.com/img.png".to_string(),
            alt: Some("an image".to_string()),
        };
        assert_eq!(image.as_text(), None);
    }
}
