//! 受信: Discord Message Create → 1 行 JSONL コールバック（said へ写す前段）。
//!
//! - production: serenity Gateway に接続し、自分以外の Message を [`crate::map::IncomingMessage`]
//!   相当の JSON 行へ機械変換して `on_line` に渡す。
//! - QC: `fake_events` fixture の追記行を同じ `on_line` へ流す（[`run_fake_events_once`]）。
//!
//! 購読集合の絞り込み（ack 済み binding の channel だけを said にする）は上位 [`crate::run`] が
//! `binding_for_address` で行う。ここは変換だけを担い、admission 判断はしない（設計 §4.3）。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

/// 偽イベントが fixture の追記を拾う間隔。
const FAKE_POLL: Duration = Duration::from_millis(25);

/// 1 行受信コールバック。real / fake 共通。
pub type OnLine = Arc<dyn Fn(String) + Send + Sync>;

/// fixture（Discord Message JSONL）の追記を tail して `on_line` に流す（QC・非 production）。
/// nostr-gateway の偽 watch と同じ tailing。EOF では待ち、追記されたら続きを読む。
pub async fn run_fake_events_once(fixture: &Path, on_line: OnLine) -> anyhow::Result<()> {
    tracing::warn!(
        fixture = %fixture.display(),
        "FAKE discord events active — streaming fixture instead of connecting serenity (QC; not production)"
    );
    let mut pos: u64 = 0;
    loop {
        if let Ok(mut file) = tokio::fs::File::open(fixture).await {
            let len = file.metadata().await?.len();
            if len < pos {
                pos = 0; // truncate/rotate は先頭から。
            }
            if len > pos {
                file.seek(std::io::SeekFrom::Start(pos)).await?;
                let mut reader = BufReader::new(file);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    let n = reader.read_line(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    if buf.ends_with('\n') {
                        pos += n as u64;
                        let line = buf.trim_end_matches(['\n', '\r']).to_string();
                        if !line.is_empty() {
                            on_line(line);
                        }
                    } else {
                        break; // 部分行は追記待ち。
                    }
                }
            }
        }
        tokio::time::sleep(FAKE_POLL).await;
    }
}

// ==================== production: serenity gateway ====================

use serenity::all::{
    Command, CommandDataOptionValue, CommandInteraction, Context, EventHandler, GatewayIntents,
    Interaction, Message as SerenityMessage, Ready,
};
use serenity::Client;

/// serenity Message → IncomingMessage 相当の JSON 行。生 ID は on_line 経由で map.rs が origin/author へ写す。
fn message_to_line(msg: &SerenityMessage) -> String {
    serde_json::json!({
        "id": msg.id.get().to_string(),
        "channel_id": msg.channel_id.get().to_string(),
        "guild_id": msg.guild_id.map(|g| g.get().to_string()),
        "author": {
            "id": msg.author.id.get().to_string(),
            "bot": msg.author.bot,
            "username": msg.author.name,
            "global_name": msg.author.global_name,
            "nickname": msg.member.as_ref().and_then(|member| member.nick.as_deref()),
        },
        "content": msg.content,
        "attachments": msg.attachments.iter().map(|attachment| serde_json::json!({
            "id": attachment.id.get().to_string(),
            "filename": attachment.filename,
            "content_type": attachment.content_type,
            "size": attachment.size,
            "url": attachment.url,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

fn model_interaction_to_line(
    interaction: &CommandInteraction,
    autocomplete: bool,
) -> Option<String> {
    if interaction.data.name != "model" {
        return None;
    }
    let option = interaction.data.options.first()?;
    let (subcommand, options) = match &option.value {
        CommandDataOptionValue::SubCommand(options) => (option.name.clone(), options),
        _ => return None,
    };
    let model = options.first().and_then(|option| match &option.value {
        CommandDataOptionValue::String(value) => Some(value.clone()),
        CommandDataOptionValue::Autocomplete { value, .. } => Some(value.clone()),
        _ => None,
    });
    let event = crate::model::ModelInteractionEvent {
        event_kind: "model_interaction".to_string(),
        interaction_id: interaction.id.get().to_string(),
        application_id: interaction.application_id.get().to_string(),
        token: interaction.token.clone(),
        guild_id: interaction.guild_id.map(|id| id.get().to_string()),
        channel_id: interaction.channel_id.get().to_string(),
        user_id: interaction.user.id.get().to_string(),
        autocomplete,
        subcommand,
        model,
    };
    serde_json::to_string(&event).ok()
}

struct Forwarder {
    on_line: OnLine,
}

#[async_trait::async_trait]
impl EventHandler for Forwarder {
    async fn ready(&self, ctx: Context, _ready: Ready) {
        if let Err(error) =
            Command::create_global_command(&ctx.http, crate::model::model_command()).await
        {
            tracing::warn!(error = %crate::secret::redact_token(&error.to_string()), "failed to register /model");
        }
    }

    async fn interaction_create(&self, _ctx: Context, interaction: Interaction) {
        let line = match interaction {
            Interaction::Command(command) => model_interaction_to_line(&command, false),
            Interaction::Autocomplete(command) => model_interaction_to_line(&command, true),
            _ => None,
        };
        if let Some(line) = line {
            (self.on_line)(line);
        }
    }

    async fn message(&self, _ctx: Context, msg: SerenityMessage) {
        // 自分自身の除外は map.rs（self_bot_id 一致）で行う。ここは全 Message を機械変換する。
        (self.on_line)(message_to_line(&msg));
    }
}

/// production の受信ループ。token は env 由来の値のみ（他へ出さない）。切断時は serenity が
/// 内部で再接続する。返るのは致命的失敗時だけ。
pub async fn run_serenity_receive(token: &str, on_line: OnLine) -> anyhow::Result<()> {
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILDS;
    let mut client = Client::builder(token, intents)
        .event_handler(Forwarder { on_line })
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "serenity client build failed: {}",
                crate::secret::redact_token(&e.to_string())
            )
        })?;
    client.start().await.map_err(|e| {
        anyhow::anyhow!(
            "serenity gateway ended: {}",
            crate::secret::redact_token(&e.to_string())
        )
    })?;
    Ok(())
}
