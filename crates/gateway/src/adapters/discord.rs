use std::sync::Arc;

use serenity::all::{CreateActionRow, CreateModal};

use anyhow::{Context as AnyhowContext, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use serenity::all::{
    ChannelId, Client, Context, CreateInteractionResponse, CreateInteractionResponseMessage,
    EventHandler, GatewayIntents, Interaction, Message as SerenityMessage, MessageId,
    ReactionType, Ready,
};
use serenity::http::Http;

use crate::message::{
    Channel, IncomingMessage, MessageSource, MessageTarget, OutgoingMessage, Sender,
};
use crate::traits::Gateway;

/// A2UI Form 用モーダル応答（ボタン押下時に `CreateInteractionResponse::Modal` で返す）。
#[derive(Clone)]
pub struct A2uiFormModalSpec {
    pub modal_custom_id: String,
    pub title: String,
    pub components: Vec<CreateActionRow>,
}

/// `custom_id` とクリックユーザー ID を受け取り、モーダルを出すべきときだけ `Some` を返す。
pub type A2uiFormModalResolver = Arc<dyn Fn(&str, &str) -> Option<A2uiFormModalSpec> + Send + Sync>;

/// Discordのコンポーネントインタラクション（ボタンクリック等）のデータ。
///
/// serenityのInteraction型からplatform-agnosticなデータに変換したもの。
/// discord crateのイベントループで実際の処理を行う。
#[derive(Debug, Clone)]
pub struct ComponentInteractionData {
    /// custom_id (例: "interaction:{uuid}:{action_name}")
    pub custom_id: String,
    /// ボタンをクリックしたユーザーのID
    pub user_id: String,
    /// ボタンをクリックしたユーザー名
    pub user_name: String,
    /// チャンネルID
    pub channel_id: String,
    /// ギルドID (DMの場合は空)
    pub guild_id: String,
    /// メッセージID
    pub message_id: String,
    /// セレクトメニューの選択値（StringSelect時のみ）
    pub selected_values: Option<Vec<String>>,
    /// モーダルSubmitのフィールド値（custom_id → value）
    pub modal_values: Option<Vec<(String, String)>>,
    /// インタラクション種別
    pub interaction_kind: InteractionKind,
}

/// コンポーネントインタラクションの種別
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionKind {
    Button,
    SelectMenu,
    ModalSubmit,
}

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
    /// A2UIコンポーネントインタラクション受信チャンネル
    interaction_rx: Mutex<mpsc::Receiver<ComponentInteractionData>>,
    interaction_tx: mpsc::Sender<ComponentInteractionData>,
    /// Form トリガーボタン → モーダル応答（未設定時は従来どおり UpdateMessage ACK のみ）
    form_modal_resolver: Option<A2uiFormModalResolver>,
}

impl DiscordGateway {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_form_modal_resolver(token, None)
    }

    /// `form_modal_resolver` を渡すと、Form に紐づくボタン押下時にモーダルで応答する。
    pub fn with_form_modal_resolver(
        token: impl Into<String>,
        form_modal_resolver: Option<A2uiFormModalResolver>,
    ) -> Self {
        let token = token.into();
        let (tx, rx) = mpsc::channel(256);
        let (interaction_tx, interaction_rx) = mpsc::channel(64);
        let http = Arc::new(Http::new(&token));
        Self {
            token,
            rx: Mutex::new(rx),
            tx,
            http,
            shard_manager: Mutex::new(None),
            interaction_rx: Mutex::new(interaction_rx),
            interaction_tx,
            form_modal_resolver,
        }
    }

    /// serenityのHTTPクライアントへの参照を返す（管理API用）
    pub fn http(&self) -> &Arc<Http> {
        &self.http
    }

    /// Bot接続を開始する（バックグラウンドタスクとして起動）
    pub async fn start(&self) -> Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILDS;

        let handler = DiscordHandler {
            tx: self.tx.clone(),
            interaction_tx: self.interaction_tx.clone(),
            self_user_id: tokio::sync::OnceCell::new(),
            form_modal_resolver: self.form_modal_resolver.clone(),
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

    /// 指定メッセージにUnicode絵文字のリアクションを付ける。
    ///
    /// 受信メッセージを処理対象として認識したことを示す 👀 などに使う。
    /// 呼び出し側は失敗を非致命的に扱うこと（権限不足・削除済みメッセージ等で失敗しうる）。
    pub async fn add_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> Result<()> {
        ChannelId::new(channel_id)
            .create_reaction(
                &self.http,
                MessageId::new(message_id),
                ReactionType::Unicode(emoji.to_string()),
            )
            .await
            .context("Failed to add reaction to Discord message")?;
        Ok(())
    }

    /// 指定チャンネルに「入力中...」インジケーターを送信する
    pub async fn start_typing(&self, channel_id: u64) -> Result<()> {
        ChannelId::new(channel_id)
            .broadcast_typing(&self.http)
            .await
            .context("Failed to broadcast typing indicator")?;
        Ok(())
    }

    /// コンポーネントインタラクション（ボタンクリック等）を受信する。
    ///
    /// Discordからのinteraction_createイベントがあるまで待機する。
    pub async fn recv_interaction(&self) -> Result<ComponentInteractionData> {
        let mut rx = self.interaction_rx.lock().await;
        rx.recv()
            .await
            .context("Discord interaction channel closed")
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
///
/// 長さは文字数（コードポイント数）で数え、長い行は文字境界で分割する。
/// バイト境界での分割はマルチバイトUTF-8（日本語等）を破壊するため行わない。
/// 空のチャンクは生成しない（Discordは空メッセージを400で拒否する）。
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;

    for line in text.lines() {
        let line_chars = line.chars().count();

        // 1行が制限を超える場合は文字境界でさらに分割
        if line_chars > max_len {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            let mut piece = String::new();
            let mut piece_chars = 0usize;
            for ch in line.chars() {
                piece.push(ch);
                piece_chars += 1;
                if piece_chars == max_len {
                    chunks.push(std::mem::take(&mut piece));
                    piece_chars = 0;
                }
            }
            if !piece.is_empty() {
                chunks.push(piece);
            }
            continue;
        }

        if current_chars + line_chars + 1 > max_len && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        if !current.is_empty() {
            current.push('\n');
            current_chars += 1;
        }
        current.push_str(line);
        current_chars += line_chars;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Discord添付ファイルが画像かどうかを判定する
fn is_image_attachment(a: &serenity::model::channel::Attachment) -> bool {
    a.content_type
        .as_deref()
        .map(|ct| ct.starts_with("image/"))
        .unwrap_or(false)
        || (a.width.is_some() && a.height.is_some())
}

// ==================== Serenity Event Handler ====================

struct DiscordHandler {
    tx: mpsc::Sender<IncomingMessage>,
    interaction_tx: mpsc::Sender<ComponentInteractionData>,
    self_user_id: tokio::sync::OnceCell<u64>,
    form_modal_resolver: Option<A2uiFormModalResolver>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, ctx: Context, msg: SerenityMessage) {
        // 自分自身のメッセージは無視（無限ループ防止）
        if let Some(self_id) = self.self_user_id.get().copied() {
            if msg.author.id.get() == self_id {
                return;
            }
        }

        info!(
            author = %msg.author.name,
            bot = msg.author.bot,
            content = %msg.content.chars().take(50).collect::<String>(),
            "Discord message event received"
        );

        let guild_id = msg.guild_id.map(|id| id.to_string()).unwrap_or_default();
        let channel_id = msg.channel_id.to_string();

        // 画像添付ファイルの処理
        let non_image_notes: Vec<String> = msg
            .attachments
            .iter()
            .filter(|a| !is_image_attachment(a))
            .map(|a| {
                let ct = a.content_type.as_deref().unwrap_or("unknown");
                format!("[添付ファイル: {} ({}), {}B]", a.filename, ct, a.size)
            })
            .collect();

        let full_text = if non_image_notes.is_empty() {
            msg.content.clone()
        } else {
            format!("{}\n{}", msg.content, non_image_notes.join("\n"))
        };

        let image_parts: Vec<crate::message::ContentPart> = msg
            .attachments
            .iter()
            .filter(|a| is_image_attachment(a))
            .map(|a| crate::message::ContentPart::Image {
                url: a.url.clone(),
                alt: Some(a.filename.clone()),
            })
            .collect();

        let content = if image_parts.is_empty() {
            crate::message::MessageContent::text(&full_text)
        } else if full_text.trim().is_empty() {
            crate::message::MessageContent::Multi(image_parts)
        } else {
            let mut parts = vec![crate::message::ContentPart::Text(full_text.clone())];
            parts.extend(image_parts);
            crate::message::MessageContent::Multi(parts)
        };

        let sender = Sender::user(msg.author.id.to_string(), &msg.author.name)
            .with_avatar(msg.author.face());

        let mut incoming = IncomingMessage::new(
            MessageSource::Discord {
                guild_id,
                channel_id: channel_id.clone(),
            },
            content,
            sender,
        )
        .with_channel(Channel {
            id: channel_id,
            name: msg.channel_id.to_string(),
        })
        .with_metadata("discord_message_id", serde_json::json!(msg.id.to_string()));

        // ギルド情報（チャンネルの場合のみ）
        if let Some(gid) = msg.guild_id {
            if let Some(guild) = ctx.cache.guild(gid) {
                incoming = incoming
                    .with_metadata("guild_name", serde_json::json!(guild.name.clone()))
                    .with_metadata(
                        "guild_icon_url",
                        serde_json::json!(guild.icon_url().unwrap_or_default()),
                    );
                if let Some(channel) = guild.channels.get(&msg.channel_id) {
                    incoming = incoming
                        .with_metadata("channel_name", serde_json::json!(channel.name.clone()));
                }
            }
        }

        if let Err(e) = self.tx.send(incoming).await {
            warn!("Failed to forward Discord message to gateway: {e}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Component(component) => {
                let custom_id = component.data.custom_id.clone();

                // Only handle our A2UI interactions (format: "interaction:{uuid}:{action}")
                if !custom_id.starts_with("interaction:") {
                    return;
                }

                debug!(
                    custom_id = %custom_id,
                    user = %component.user.name,
                    "Discord component interaction received"
                );

                let guild_id = component
                    .guild_id
                    .map(|id| id.to_string())
                    .unwrap_or_default();
                let channel_id = component.channel_id.to_string();
                let message_id = component.message.id.to_string();
                let user_id = component.user.id.to_string();
                let user_name = component.user.name.clone();

                // Detect interaction kind and extract select values
                use serenity::all::ComponentInteractionDataKind;
                let (interaction_kind, selected_values) = match &component.data.kind {
                    ComponentInteractionDataKind::StringSelect { values } => {
                        (InteractionKind::SelectMenu, Some(values.clone()))
                    }
                    ComponentInteractionDataKind::Button => {
                        (InteractionKind::Button, None)
                    }
                    _ => (InteractionKind::Button, None),
                };

                // Form トリガー: モーダルで応答（UpdateMessage だとモーダルが開けない）
                if interaction_kind == InteractionKind::Button {
                    if let Some(ref resolver) = self.form_modal_resolver {
                        if let Some(spec) =
                            resolver(&custom_id, &component.user.id.to_string())
                        {
                            let modal = CreateModal::new(
                                spec.modal_custom_id.clone(),
                                spec.title.clone(),
                            )
                            .components(spec.components.clone());
                            let _ = component
                                .create_response(
                                    &ctx.http,
                                    CreateInteractionResponse::Modal(modal),
                                )
                                .await;
                            return;
                        }
                    }
                }

                // ACK with deferred update (prevents "interaction failed" message)
                let _ = component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new(),
                        ),
                    )
                    .await;

                let data = ComponentInteractionData {
                    custom_id,
                    user_id,
                    user_name,
                    channel_id,
                    guild_id,
                    message_id,
                    selected_values,
                    modal_values: None,
                    interaction_kind,
                };

                if let Err(e) = self.interaction_tx.send(data).await {
                    warn!("Failed to forward component interaction: {e}");
                }
            }
            Interaction::Modal(modal) => {
                let custom_id = modal.data.custom_id.clone();

                // Only handle our A2UI modals
                if !custom_id.starts_with("interaction:") {
                    return;
                }

                debug!(
                    custom_id = %custom_id,
                    user = %modal.user.name,
                    "Discord modal submit received"
                );

                let guild_id = modal
                    .guild_id
                    .map(|id| id.to_string())
                    .unwrap_or_default();
                let channel_id = modal.channel_id.to_string();
                let user_id = modal.user.id.to_string();
                let user_name = modal.user.name.clone();

                // Extract modal field values
                let mut modal_values = Vec::new();
                for row in &modal.data.components {
                    for comp in &row.components {
                        if let serenity::all::ActionRowComponent::InputText(input) = comp {
                            modal_values.push((
                                input.custom_id.clone(),
                                input.value.clone().unwrap_or_default(),
                            ));
                        }
                    }
                }

                // ACK the modal submit
                let _ = modal
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new(),
                        ),
                    )
                    .await;

                let data = ComponentInteractionData {
                    custom_id,
                    user_id,
                    user_name,
                    channel_id,
                    guild_id,
                    message_id: String::new(), // Modal submits don't have a message_id
                    selected_values: None,
                    modal_values: Some(modal_values),
                    interaction_kind: InteractionKind::ModalSubmit,
                };

                if let Err(e) = self.interaction_tx.send(data).await {
                    warn!("Failed to forward modal submit: {e}");
                }
            }
            _ => return,
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        let self_id = ready.user.id.get();
        let _ = self.self_user_id.set(self_id);
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
    fn form_modal_spec_builds_serenity_modal() {
        let spec = A2uiFormModalSpec {
            modal_custom_id: "interaction:uuid-1:modal:submit".into(),
            title: "Form title".into(),
            components: vec![CreateActionRow::InputText(
                serenity::all::CreateInputText::new(
                    serenity::all::InputTextStyle::Short,
                    "Field",
                    "field_id",
                ),
            )],
        };
        let _modal = CreateModal::new(&spec.modal_custom_id, &spec.title)
            .components(spec.components.clone());
    }

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
    fn test_split_message_long_japanese_no_corruption() {
        // 2000文字超の日本語1行が文字境界で分割され、U+FFFDが混入しないこと。
        let text = "あ".repeat(2500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks[1].chars().count(), 500);
        for chunk in &chunks {
            assert!(!chunk.contains('\u{FFFD}'), "no replacement characters");
            assert!(!chunk.is_empty(), "no empty chunks");
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_split_message_exact_boundary_no_empty_chunk() {
        // ちょうど max_len の行で空チャンクが生成されないこと。
        let text = "a".repeat(200);
        let chunks = split_message(&text, 200);
        assert_eq!(chunks.len(), 1);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn test_split_message_multiline() {
        let lines: Vec<String> = (0..100)
            .map(|i| format!("Line {i}: some content here"))
            .collect();
        let text = lines.join("\n");
        let chunks = split_message(&text, 200);
        for chunk in &chunks {
            assert!(chunk.len() <= 200);
        }
    }
}
