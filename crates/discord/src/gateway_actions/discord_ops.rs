//! Discord固有のI/O操作 (list_guilds, list_channels, add_reaction, send_file)。
//! `discord_channel_config` の書き込みは core（`apply_discord_channel_config`）へ委譲する。

use std::path::{Path, PathBuf};

use serde_json::json;
use serenity::all::{ChannelId, CreateAttachment, CreateChannel, CreateMessage, CreateWebhook};
use serenity::model::channel::ReactionType;
use serenity::model::id::{GuildId, MessageId};
use serenity::model::prelude::ChannelType;
use tracing::{debug, error};

use opencrab_gateway::{GatewayActionResult, GatewayCallContext};

use super::DiscordGatewayActions;

impl DiscordGatewayActions {
    pub(crate) async fn execute_list_guilds(&self) -> GatewayActionResult {
        match self.http.get_guilds(None, None).await {
            Ok(guilds) => {
                debug!("Got {} guilds from Discord API", guilds.len());
                let guild_list: Vec<serde_json::Value> = guilds
                    .into_iter()
                    .map(|g| {
                        json!({
                            "id": g.id.to_string(),
                            "name": g.name,
                            "member_count": serde_json::Value::Null,
                        })
                    })
                    .collect();
                let count = guild_list.len();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "guilds": guild_list,
                        "count": count,
                    })),
                    error: None,
                }
            }
            Err(e) => {
                error!("Discord API get_guilds failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "サーバー一覧の取得に失敗: Failed to get guilds: {e}"
                    )),
                }
            }
        }
    }

    pub(crate) async fn execute_list_channels(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let guild_id = match args.get("guild_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("guild_idパラメータが必要です".to_string()),
                }
            }
        };

        let gid: u64 = match guild_id.parse() {
            Ok(id) => id,
            Err(_) => {
                error!("Invalid guild_id passed to list_channels: {guild_id}");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "guild_idが数値IDではありません: '{guild_id}' — guild名ではなくdiscord_list_guildsで取得したIDを使ってください"
                    )),
                };
            }
        };

        let channels = match self
            .http
            .get_channels(serenity::model::id::GuildId::new(gid))
            .await
        {
            Ok(chs) => chs,
            Err(e) => {
                error!("Discord API get_channels failed for guild {gid}: {e}");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "チャンネル一覧の取得に失敗: Failed to get channels for guild {gid}: {e}"
                    )),
                };
            }
        };

        debug!(
            "Got {} channels from guild {gid}, {} are text",
            channels.len(),
            channels
                .iter()
                .filter(|c| c.kind == ChannelType::Text)
                .count()
        );

        // DB設定も合わせて取得
        let db_configs = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::list_channel_configs_by_guild(&conn, guild_id).unwrap_or_default()
        };

        let channel_list: Vec<serde_json::Value> = channels
            .into_iter()
            .filter(|ch| ch.kind == ChannelType::Text)
            .map(|ch| {
                let ch_id = ch.id.to_string();
                // エージェント固有設定を優先、なければグローバル設定（agent_id=""）
                let db_cfg = db_configs
                    .iter()
                    .find(|c| c.channel_id == ch_id && c.agent_id == ctx.agent_id)
                    .or_else(|| {
                        db_configs
                            .iter()
                            .find(|c| c.channel_id == ch_id && c.agent_id.is_empty())
                    });
                let readable = db_cfg.map(|c| c.readable).unwrap_or(true);
                let writable = db_cfg.map(|c| c.writable).unwrap_or(true);
                let whitelisted = db_cfg.map(|c| c.whitelisted).unwrap_or(false);

                json!({
                    "id": ch_id,
                    "name": ch.name,
                    "kind": "text",
                    "readable": readable,
                    "writable": writable,
                    "whitelisted": whitelisted,
                })
            })
            .collect();

        let count = channel_list.len();
        GatewayActionResult {
            success: true,
            data: Some(json!({
                "guild_id": guild_id,
                "channels": channel_list,
                "count": count,
            })),
            error: None,
        }
    }

    pub(crate) fn execute_discord_channel_config(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        opencrab_actions::apply_discord_channel_config(&self.db, args, &ctx.agent_id)
    }

    pub(crate) async fn execute_discord_create_webhook(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let channel_id_str = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("channel_idパラメータが必要です".to_string()),
                }
            }
        };

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("無効なchannel_id: {channel_id_str}")),
                }
            }
        };

        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("opencrab-subtask");

        if name.chars().count() < 2 || name.chars().count() > 80 {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("webhook名は2〜80文字で指定してください".to_string()),
            };
        }

        let webhook = match ChannelId::new(channel_id)
            .create_webhook(
                self.http.clone(),
                CreateWebhook::new(name).audit_log_reason("opencrab discord_create_webhook"),
            )
            .await
        {
            Ok(webhook) => webhook,
            Err(e) => {
                error!("Discord create_webhook failed for channel {channel_id}: {e}");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "webhookの作成に失敗: BotにManage Webhooks権限があるか確認してください: {e}"
                    )),
                };
            }
        };

        let webhook_value = match serde_json::to_value(&webhook) {
            Ok(value) => value,
            Err(e) => {
                error!("Failed to serialize created webhook metadata: {e}");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("作成したwebhook情報の処理に失敗: {e}")),
                };
            }
        };

        let webhook_id = webhook.id.to_string();
        let token = webhook_value
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let url = webhook_value
            .get("url")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if token.is_empty() {
                    None
                } else {
                    Some(format!(
                        "https://discord.com/api/webhooks/{webhook_id}/{token}"
                    ))
                }
            });

        let url = match url {
            Some(url) => url,
            None => {
                error!("Discord create_webhook response did not include a token");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "webhookは作成されましたが、Discord API応答にtokenがありませんでした"
                            .to_string(),
                    ),
                };
            }
        };

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "channel_id": channel_id_str,
                "webhook_id": webhook_id,
                "name": webhook.name.unwrap_or_else(|| name.to_string()),
                "url": url,
                "message": "webhookを作成しました。このurlをspawn_subtask.webhook.urlに渡せます。",
            })),
            error: None,
        }
    }

    pub(crate) async fn execute_discord_create_channel(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        // guild_id は必須（このレイヤーではデフォルトサーバーを解決できない）
        let guild_id_str = match args.get("guild_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "guild_idパラメータが必要です（デフォルトサーバーは解決できないため、discord_list_guildsで取得した数値IDを指定してください）"
                            .to_string(),
                    ),
                }
            }
        };

        let guild_id: u64 = match guild_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                error!("Invalid guild_id passed to create_channel: {guild_id_str}");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "guild_idが数値IDではありません: '{guild_id_str}' — guild名ではなくdiscord_list_guildsで取得したIDを使ってください"
                    )),
                };
            }
        };

        let name = match args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(n) => n,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("nameパラメータが必要です".to_string()),
                }
            }
        };

        if name.chars().count() < 2 || name.chars().count() > 100 {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("チャンネル名は2〜100文字で指定してください".to_string()),
            };
        }

        // parent_id は任意。指定された場合のみ数値IDとして解析する。
        let parent_id: Option<u64> = match args.get("parent_id").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => match p.trim().parse() {
                Ok(id) => Some(id),
                Err(_) => {
                    return GatewayActionResult {
                        success: false,
                        data: None,
                        error: Some(format!("parent_idが数値IDではありません: '{p}'")),
                    }
                }
            },
            _ => None,
        };

        let topic = args
            .get("topic")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("opencrab discord_create_channel");

        let mut builder = CreateChannel::new(name)
            .kind(ChannelType::Text)
            .audit_log_reason(reason);
        if let Some(pid) = parent_id {
            builder = builder.category(ChannelId::new(pid));
        }
        if let Some(t) = topic {
            builder = builder.topic(t);
        }

        let channel = match GuildId::new(guild_id)
            .create_channel(self.http.clone(), builder)
            .await
        {
            Ok(ch) => ch,
            Err(e) => {
                error!("Discord create_channel failed for guild {guild_id}: {e}");
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!(
                        "チャンネルの作成に失敗: BotにManage Channels権限があるか確認してください: {e}"
                    )),
                };
            }
        };

        let channel_id = channel.id.to_string();
        let parent_id_value = channel
            .parent_id
            .map(|p| serde_json::Value::String(p.to_string()))
            .unwrap_or(serde_json::Value::Null);
        let url = format!("https://discord.com/channels/{guild_id}/{channel_id}");

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "id": channel_id,
                "name": channel.name,
                "guild_id": channel.guild_id.to_string(),
                "parent_id": parent_id_value,
                "url": url,
                "message": format!("チャンネル {} を作成しました", channel.name),
            })),
            error: None,
        }
    }

    pub(crate) async fn execute_discord_add_reaction(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let channel_id_str = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("channel_idパラメータが必要です".to_string()),
                }
            }
        };
        let message_id_str = match args.get("message_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("message_idパラメータが必要です".to_string()),
                }
            }
        };
        let emoji_str = match args.get("emoji").and_then(|v| v.as_str()) {
            Some(e) => e,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("emojiパラメータが必要です".to_string()),
                }
            }
        };

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("無効なchannel_id: {channel_id_str}")),
                }
            }
        };
        let message_id: u64 = match message_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("無効なmessage_id: {message_id_str}")),
                }
            }
        };

        // 絵文字の解析: "name:id" 形式ならカスタム絵文字、それ以外はUnicode
        let reaction = if let Some(colon_pos) = emoji_str.find(':') {
            let name = &emoji_str[..colon_pos];
            let id_str = &emoji_str[colon_pos + 1..];
            match id_str.parse::<u64>() {
                Ok(emoji_id) => ReactionType::Custom {
                    animated: false,
                    id: serenity::model::id::EmojiId::new(emoji_id),
                    name: Some(name.to_string()),
                },
                Err(_) => ReactionType::Unicode(emoji_str.to_string()),
            }
        } else {
            ReactionType::Unicode(emoji_str.to_string())
        };

        match self
            .http
            .create_reaction(
                serenity::model::id::ChannelId::new(channel_id),
                MessageId::new(message_id),
                &reaction,
            )
            .await
        {
            Ok(()) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "channel_id": channel_id_str,
                    "message_id": message_id_str,
                    "emoji": emoji_str,
                    "message": format!("リアクション {} を追加しました", emoji_str),
                })),
                error: None,
            },
            Err(e) => {
                error!("Discord create_reaction failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("リアクションの追加に失敗: {e}")),
                }
            }
        }
    }

    pub(crate) async fn execute_send_file(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let channel_id_str = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("channel_idパラメータが必要です".to_string()),
                }
            }
        };
        let file_path_str = match args.get("file_path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("file_pathパラメータが必要です".to_string()),
                }
            }
        };
        let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
        let filename_override = args.get("filename").and_then(|v| v.as_str());

        let channel_id: u64 = match channel_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("無効なchannel_id: {channel_id_str}")),
                }
            }
        };

        // Workspace path validation (security: prevent path traversal)
        let workspace_root = match self
            .agent_workspace_root(&ctx.agent_id)
            .and_then(|p| p.canonicalize().map_err(anyhow::Error::from))
        {
            Ok(p) => p,
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("ワークスペースのパス解決に失敗: {e}")),
                }
            }
        };

        let abs_path = if Path::new(file_path_str).is_absolute() {
            PathBuf::from(file_path_str)
        } else {
            workspace_root.join(file_path_str)
        };

        let canonical = match abs_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("ファイルが見つかりません: {file_path_str}: {e}")),
                }
            }
        };

        if !canonical.starts_with(&workspace_root) {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "ワークスペース外のファイルは送信できません: {file_path_str}"
                )),
            };
        }

        // File size check (25MB limit)
        let metadata = match tokio::fs::metadata(&canonical).await {
            Ok(m) => m,
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("ファイル情報の取得に失敗: {e}")),
                }
            }
        };
        const MAX_FILE_SIZE: u64 = 25 * 1024 * 1024;
        if metadata.len() > MAX_FILE_SIZE {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "ファイルサイズが25MB制限を超えています: {}bytes",
                    metadata.len()
                )),
            };
        }

        // Build attachment
        let mut attachment = match CreateAttachment::path(&canonical).await {
            Ok(a) => a,
            Err(e) => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("ファイル読み込み失敗: {e}")),
                }
            }
        };

        // Apply filename override if specified (レビューメモ: display_nameに設定する)
        let display_name = if let Some(fname) = filename_override {
            attachment.filename = fname.to_string();
            fname.to_string()
        } else {
            canonical
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
                .to_string()
        };

        // Build message
        let mut msg = CreateMessage::new().add_file(attachment);
        if !caption.is_empty() {
            msg = msg.content(caption);
        }

        // Send
        match ChannelId::new(channel_id)
            .send_message(&self.http, msg)
            .await
        {
            Ok(_) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "channel_id": channel_id_str,
                    "file": display_name,
                    "message": format!("ファイル {} を送信しました", display_name),
                })),
                error: None,
            },
            Err(e) => {
                error!("Discord send_message (file) failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("ファイル送信に失敗: {e}")),
                }
            }
        }
    }
}
