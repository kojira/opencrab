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

/// メッセージ送信者。
///
/// **送信者が bot かどうかは持たない。** 無限ループを止めるのは「自分自身の投稿か」
/// の判定（受信側の transport が自分の発言を除外する）であって、bot フラグでは
/// ない。bot を別扱いすると**エージェント同士が会話できなくなる**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

impl Sender {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
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
    pub fn new(source: MessageSource, content: MessageContent, sender: Sender) -> Self {
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
            Sender::new("user-1", "User One"),
        );
        assert!(!msg.id.is_empty());
        assert_eq!(msg.content.as_text(), Some("test message"));
    }

    /// **送信者に「bot か人間か」の区別を持ち込まない。**
    ///
    /// 無限ループを防ぐのは「自分自身の投稿か」の判定（受信側の transport が自分の
    /// 発言を除外する）であって、bot フラグではない。bot を別扱いすると
    /// **エージェント同士が会話できなくなる**（実際 👀 が付かず、VC の声も拾えなかった）。
    /// フィールドを 1 つ足すだけで「bot なら〜」の分岐が復活するので、形で塞ぐ。
    #[test]
    fn sender_does_not_classify_the_author() {
        let value = serde_json::to_value(Sender::new("u-1", "だれか")).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("Sender は object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["avatar_url", "id", "name"],
            "送信者に bot/人間の区別が復活している（bot を特別扱いするとエージェント同士が会話できない）"
        );
    }

    #[test]
    fn test_message_content_as_text() {
        assert_eq!(
            MessageContent::Text("hello".to_string()).as_text(),
            Some("hello")
        );
        let image = MessageContent::Image {
            url: "http://example.com/img.png".to_string(),
            alt: Some("an image".to_string()),
        };
        assert_eq!(image.as_text(), None);
    }
}
