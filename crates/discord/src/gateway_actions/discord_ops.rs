//! Discord固有のI/O操作 (list_guilds, list_channels, channel_config, add_reaction, send_file)

use std::path::{Path, PathBuf};

use serde_json::json;
use serenity::all::{ChannelId, CreateAttachment, CreateMessage};
use serenity::model::channel::ReactionType;
use serenity::model::id::MessageId;
use serenity::model::prelude::ChannelType;
use tracing::{debug, error};

use opencrab_gateway::GatewayActionResult;

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
                    .find(|c| c.channel_id == ch_id && c.agent_id == self.agent_id)
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
    ) -> GatewayActionResult {
        let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("channel_idパラメータが必要です".to_string()),
                }
            }
        };
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
        let channel_name = args
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let readable = match args.get("readable").and_then(|v| v.as_bool()) {
            Some(r) => r,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("readableパラメータが必要です".to_string()),
                }
            }
        };
        let writable = match args.get("writable").and_then(|v| v.as_bool()) {
            Some(w) => w,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("writableパラメータが必要です".to_string()),
                }
            }
        };

        let whitelisted = args
            .get("whitelisted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let heartbeat_enabled = args
            .get("heartbeat_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let cfg = opencrab_db::queries::ChannelConfigRow {
            channel_id: channel_id.to_string(),
            agent_id: self.agent_id.clone(),
            guild_id: guild_id.to_string(),
            channel_name: channel_name.to_string(),
            readable,
            writable,
            whitelisted,
            heartbeat_enabled,
            heartbeat_interval_secs: None,
        };

        let result = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::upsert_channel_config(&conn, &cfg)
        };

        match result {
            Ok(()) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "channel_id": channel_id,
                    "channel_name": channel_name,
                    "readable": readable,
                    "writable": writable,
                    "whitelisted": whitelisted,
                    "message": format!(
                        "チャンネル {} の設定を更新しました (readable={}, writable={})",
                        if channel_name.is_empty() { channel_id } else { channel_name },
                        readable,
                        writable,
                    ),
                })),
                error: None,
            },
            Err(e) => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("チャンネル設定の保存に失敗: {e}")),
            },
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

    pub(crate) async fn execute_send_file(&self, args: &serde_json::Value) -> GatewayActionResult {
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
        let workspace_root = match self.workspace_root.canonicalize() {
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
