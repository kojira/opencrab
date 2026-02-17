use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tracing::debug;

use crate::message::{
    IncomingMessage, MessageContent, MessageSource, OutgoingMessage, Sender,
};
use crate::traits::Gateway;

/// CLIゲートウェイ
///
/// 標準入出力を使ったインタラクティブなメッセージング。
/// stdinからユーザー入力を読み取り、stdoutにレスポンスを出力する。
pub struct CliGateway {
    session_id: String,
    user_name: String,
    reader: Option<BufReader<io::Stdin>>,
}

impl CliGateway {
    /// 新しいCliGatewayを作成する
    pub fn new(user_name: impl Into<String>) -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            user_name: user_name.into(),
            reader: None,
        }
    }

    /// セッションIDを指定して作成
    pub fn with_session_id(
        user_name: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            user_name: user_name.into(),
            reader: None,
        }
    }
}

#[async_trait]
impl Gateway for CliGateway {
    fn name(&self) -> &str {
        "cli"
    }

    async fn receive(&mut self) -> Result<IncomingMessage> {
        let reader = self
            .reader
            .as_mut()
            .context("CLI gateway not connected. Call connect() first.")?;

        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .await
            .context("Failed to read from stdin")?;

        if bytes_read == 0 {
            anyhow::bail!("stdin closed (EOF)");
        }

        let content = line.trim_end().to_string();
        debug!(content = %content, "CLI received input");

        let message = IncomingMessage::new(
            MessageSource::Cli {
                session_id: self.session_id.clone(),
            },
            MessageContent::text(content),
            Sender::user("cli-user", &self.user_name),
        );

        Ok(message)
    }

    async fn send(&self, message: OutgoingMessage) -> Result<()> {
        let text = match &message.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Image { url, alt } => {
                format!("[Image: {}]", alt.as_deref().unwrap_or(url.as_str()))
            }
            MessageContent::Multi(parts) => {
                let mut output = String::new();
                for part in parts {
                    match part {
                        crate::message::ContentPart::Text(s) => output.push_str(s),
                        crate::message::ContentPart::Image { url, alt } => {
                            output.push_str(&format!(
                                "[Image: {}]",
                                alt.as_deref().unwrap_or(url.as_str())
                            ));
                        }
                    }
                }
                output
            }
        };

        println!("{}", text);
        Ok(())
    }

    async fn connect(&mut self) -> Result<()> {
        self.reader = Some(BufReader::new(io::stdin()));
        debug!(session_id = %self.session_id, "CLI gateway connected");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.reader = None;
        debug!(session_id = %self.session_id, "CLI gateway disconnected");
        Ok(())
    }
}
