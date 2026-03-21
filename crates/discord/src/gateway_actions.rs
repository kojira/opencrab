//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! Discord管理操作（サーバー一覧、チャンネル一覧、チャンネル設定）を
//! ゲートウェイ固有アクションとして提供する。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::prelude::ChannelType;
use serenity::model::id::MessageId;
use serenity::model::channel::ReactionType;
use serde_json::json;
use tracing::{debug, error};
use opencrab_gateway::{GatewayActions, GatewayActionDef, GatewayActionResult};

/// Discord固有のゲートウェイアクション実装。
///
/// serenityのHTTPクライアントとDB接続を保持し、
/// Discord管理操作をGatewayActionsとして提供する。
pub struct DiscordGatewayActions {
    http: Arc<Http>,
    db: Arc<Mutex<rusqlite::Connection>>,
    agent_id: String,
    tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
    llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
    default_model: String,
}

impl DiscordGatewayActions {
    pub fn new(
        http: Arc<Http>,
        db: Arc<Mutex<rusqlite::Connection>>,
        agent_id: String,
        tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
        llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
        default_model: String,
    ) -> Self {
        Self { http, db, agent_id, tools_config, llm_client, default_model }
    }

    async fn execute_list_guilds(&self) -> GatewayActionResult {
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
                    error: Some(format!("サーバー一覧の取得に失敗: Failed to get guilds: {e}")),
                }
            }
        }
    }

    async fn execute_list_channels(&self, args: &serde_json::Value) -> GatewayActionResult {
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
                    error: Some(format!("チャンネル一覧の取得に失敗: Failed to get channels for guild {gid}: {e}")),
                };
            }
        };

        debug!(
            "Got {} channels from guild {gid}, {} are text",
            channels.len(),
            channels.iter().filter(|c| c.kind == ChannelType::Text).count()
        );

        // DB設定も合わせて取得
        let db_configs = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::list_channel_configs_by_guild(&conn, guild_id)
                .unwrap_or_default()
        };

        let channel_list: Vec<serde_json::Value> = channels
            .into_iter()
            .filter(|ch| ch.kind == ChannelType::Text)
            .map(|ch| {
                let ch_id = ch.id.to_string();
                let db_cfg = db_configs.iter().find(|c| c.channel_id == ch_id);
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

    fn execute_channel_config(&self, args: &serde_json::Value) -> GatewayActionResult {
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
    async fn execute_add_reaction(&self, args: &serde_json::Value) -> GatewayActionResult {
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

        match self.http.create_reaction(
            serenity::model::id::ChannelId::new(channel_id),
            MessageId::new(message_id),
            &reaction,
        ).await {
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

    fn execute_list_duplicate_skills(&self) -> GatewayActionResult {
        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::find_duplicate_skills(&conn, &self.agent_id) {
            Ok(duplicates) => {
                let list: Vec<serde_json::Value> = duplicates
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "name": s.name,
                            "description": s.description,
                            "usage_count": s.usage_count,
                        })
                    })
                    .collect();
                let count = list.len();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "duplicates": list,
                        "count": count,
                        "agent_id": self.agent_id,
                    })),
                    error: None,
                }
            }
            Err(e) => {
                error!("find_duplicate_skills failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("重複スキルの取得に失敗: {e}")),
                }
            }
        }
    }

    fn execute_merge_skills(&self, args: &serde_json::Value) -> GatewayActionResult {
        let source_id = match args.get("source_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("source_idパラメータが必要です".to_string()),
                }
            }
        };
        let target_id = match args.get("target_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("target_idパラメータが必要です".to_string()),
                }
            }
        };

        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::merge_skills(&conn, source_id, target_id) {
            Ok(()) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "source_id": source_id,
                    "target_id": target_id,
                    "message": format!("スキル {} を {} にマージしました", source_id, target_id),
                })),
                error: None,
            },
            Err(e) => {
                error!("merge_skills failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("スキルのマージに失敗: {e}")),
                }
            }
        }
    }

    fn execute_update_memory_index_config(&self, args: &serde_json::Value) -> GatewayActionResult {
        let batch_size = args.get("batch_size").and_then(|v| v.as_i64());
        let threshold = args.get("threshold").and_then(|v| v.as_i64());

        if batch_size.is_none() && threshold.is_none() {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("batch_sizeまたはthresholdの少なくとも1つが必要です".to_string()),
            };
        }

        let conn = self.db.lock().unwrap();

        let current = opencrab_db::queries::get_memory_index_config(&conn, &self.agent_id);
        let (current_batch_size, current_threshold) = match &current {
            Ok(cfg) => (cfg.batch_size, cfg.threshold),
            Err(_) => (
                opencrab_db::queries::BATCH_SIZE_DEFAULT,
                opencrab_db::queries::THRESHOLD_DEFAULT,
            ),
        };

        let new_batch_size = batch_size.unwrap_or(current_batch_size);
        let new_threshold = threshold.unwrap_or(current_threshold);

        match opencrab_db::queries::upsert_memory_index_config(
            &conn,
            &self.agent_id,
            new_batch_size,
            new_threshold,
        ) {
            Ok(updated) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "agent_id": self.agent_id,
                    "previous": {
                        "batch_size": current_batch_size,
                        "threshold": current_threshold,
                    },
                    "current": {
                        "batch_size": updated.batch_size,
                        "threshold": updated.threshold,
                    },
                })),
                error: None,
            },
            Err(e) => {
                error!("upsert_memory_index_config failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("メモリインデックス設定の更新に失敗: {e}")),
                }
            }
        }
    }

    async fn execute_rebuild_memory_index(&self) -> GatewayActionResult {
        let llm_client = match &self.llm_client {
            Some(client) => client.clone(),
            None => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("LLMクライアントが設定されていません".to_string()),
                }
            }
        };

        let config = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::get_memory_index_config(&conn, &self.agent_id)
                .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                    agent_id: self.agent_id.clone(),
                    batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                    threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                    updated_at: String::new(),
                })
        };

        match opencrab_core::memory_index::IndexBuilder::rebuild_index(
            &self.db,
            &self.agent_id,
            llm_client.as_ref(),
            &self.default_model,
            config.batch_size as usize,
        )
        .await
        {
            Ok(result) => GatewayActionResult {
                success: true,
                data: Some(serde_json::json!({
                    "agent_id": self.agent_id,
                    "logs_indexed": result.logs_indexed,
                    "nodes_created": result.nodes_created,
                    "message": format!(
                        "メモリインデックスを再構築しました（{}件のログ → {}ノード作成）",
                        result.logs_indexed,
                        result.nodes_created,
                    ),
                })),
                error: None,
            },
            Err(e) => {
                error!("rebuild_memory_index failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("メモリインデックスの再構築に失敗: {e}")),
                }
            }
        }
    }

    fn execute_add_allowed_command(&self, args: &serde_json::Value) -> GatewayActionResult {
        let caller = args.get("__caller").and_then(|v| v.as_str()).unwrap_or("agent");
        if caller != "owner" {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはオーナーのみ実行できます".to_string()),
            };
        }

        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("commandパラメータが必要です".to_string()),
                }
            }
        };

        if !command.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "コマンド名に無効な文字が含まれています: {}（英数字・ハイフン・アンダースコアのみ使用可）",
                    command
                )),
            };
        }

        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::add_agent_allowed_command(&conn, &self.agent_id, command, "owner") {
            Ok(true) => {
                // Update in-memory tools_config
                drop(conn);
                if let Ok(mut cfg) = self.tools_config.write() {
                    if let Some(ref mut shell) = cfg.shell {
                        let cmd_str = command.to_string();
                        if !shell.allowed_commands.contains(&cmd_str) {
                            shell.allowed_commands.push(cmd_str);
                        }
                    }
                }
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "command": command,
                        "agent_id": self.agent_id,
                        "message": format!("`{}` を許可コマンドに追加しました", command),
                    })),
                    error: None,
                }
            }
            Ok(false) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "command": command,
                    "agent_id": self.agent_id,
                    "message": format!("`{}` はすでに許可コマンドに登録されています", command),
                    "already_exists": true,
                })),
                error: None,
            },
            Err(e) => {
                error!("add_agent_allowed_command failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("許可コマンドの追加に失敗: {e}")),
                }
            }
        }
    }

    fn execute_list_allowed_commands(&self) -> GatewayActionResult {
        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::list_agent_allowed_commands(&conn, &self.agent_id) {
            Ok(commands) => {
                let count = commands.len();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "commands": commands,
                        "count": count,
                        "agent_id": self.agent_id,
                    })),
                    error: None,
                }
            }
            Err(e) => {
                error!("list_agent_allowed_commands failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("許可コマンドの取得に失敗: {e}")),
                }
            }
        }
    }

    fn execute_remove_allowed_command(&self, args: &serde_json::Value) -> GatewayActionResult {
        let caller = args.get("__caller").and_then(|v| v.as_str()).unwrap_or("agent");
        if caller != "owner" {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはオーナーのみ実行できます".to_string()),
            };
        }

        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c,
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("commandパラメータが必要です".to_string()),
                }
            }
        };

        let conn = self.db.lock().unwrap();
        match opencrab_db::queries::remove_agent_allowed_command(&conn, &self.agent_id, command) {
            Ok(true) => {
                // Update in-memory tools_config
                drop(conn);
                if let Ok(mut cfg) = self.tools_config.write() {
                    if let Some(ref mut shell) = cfg.shell {
                        shell.allowed_commands.retain(|c| c != command);
                    }
                }
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "command": command,
                        "agent_id": self.agent_id,
                        "message": format!("`{}` を許可コマンドから削除しました", command),
                    })),
                    error: None,
                }
            }
            Ok(false) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "command": command,
                    "agent_id": self.agent_id,
                    "message": format!("`{}` は許可コマンドに登録されていませんでした", command),
                    "not_found": true,
                })),
                error: None,
            },
            Err(e) => {
                error!("remove_agent_allowed_command failed: {e}");
                GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("許可コマンドの削除に失敗: {e}")),
                }
            }
        }
    }

    fn execute_create_skill(&self, args: &serde_json::Value) -> GatewayActionResult {
        let caller = args.get("__caller").and_then(|v| v.as_str()).unwrap_or("agent");
        if caller != "owner" && caller != "co_agent" && caller != "trusted_user" {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("このアクションはtrusted userのみ実行できます".to_string()),
            };
        }
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("name is required".to_string()),
            },
        };
        let description = match args.get("description").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("description is required".to_string()),
            },
        };
        let guidance = args.get("guidance").and_then(|v| v.as_str()).unwrap_or("");

        let conn = self.db.lock().unwrap();

        // Deduplication: check if skill with same name exists (non-archived)
        if let Ok(Some(existing)) = opencrab_db::queries::find_skill_by_name(&conn, &self.agent_id, name) {
            let mut updated = existing;
            updated.description = description.to_string();
            updated.guidance = guidance.to_string();
            if let Err(e) = opencrab_db::queries::update_skill(&conn, &updated) {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Failed to update existing skill: {e}")),
                };
            }
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "id": updated.id,
                    "name": name,
                    "action": "updated"
                })),
                error: None,
            };
        }

        // Check archived skills
        if let Ok(Some(existing)) = opencrab_db::queries::find_skill_by_name_any(&conn, &self.agent_id, name) {
            let mut updated = existing;
            updated.archived = false;
            updated.is_active = true;
            updated.description = description.to_string();
            updated.guidance = guidance.to_string();
            if let Err(e) = opencrab_db::queries::update_skill(&conn, &updated) {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Failed to restore archived skill: {e}")),
                };
            }
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "id": updated.id,
                    "name": name,
                    "action": "restored"
                })),
                error: None,
            };
        }

        let id = uuid::Uuid::new_v4().to_string();
        let row = opencrab_db::queries::SkillRow {
            id: id.clone(),
            agent_id: self.agent_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            situation_pattern: String::new(),
            guidance: guidance.to_string(),
            source_type: "acquired".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "\"agent\"".to_string(),
            archived: false,
        };

        if let Err(e) = opencrab_db::queries::insert_skill(&conn, &row) {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Failed to create skill: {e}")),
            };
        }

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "id": id,
                "name": name,
                "action": "created"
            })),
            error: None,
        }
    }

}

#[async_trait]
impl GatewayActions for DiscordGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "discord_list_guilds".to_string(),
                description: "Botが参加しているDiscordサーバー（guild）の一覧を取得する。返り値の各サーバーの `id` フィールド（数値文字列）を、他のアクションの `guild_id` パラメータとして使用すること。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "discord_list_channels".to_string(),
                description: "指定サーバーのテキストチャンネル一覧と、各チャンネルの現在のreadable/writable/whitelisted設定を取得する。チャンネルの `id` フィールドを discord_channel_config の channel_id として使用すること。guild_id は discord_list_guilds で取得した数値IDを指定。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "guild_id": {
                            "type": "string",
                            "description": "対象サーバーの数値ID（discord_list_guildsの結果から取得）。サーバー名ではなくIDを指定すること。"
                        }
                    },
                    "required": ["guild_id"]
                }),
            },
            GatewayActionDef {
                name: "discord_channel_config".to_string(),
                description: "Discordチャンネルの読み書き設定を変更する。readableをfalseにするとそのチャンネルのメッセージを無視し、writableをfalseにすると返信しない。whitelisted=trueにするとホワイトリストに登録され、そのチャンネルからのメッセージを優先処理する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "対象チャンネルの数値ID（discord_list_channelsの結果から取得）。チャンネル名ではなくIDを指定すること。"
                        },
                        "guild_id": {
                            "type": "string",
                            "description": "チャンネルが属するサーバーの数値ID（discord_list_guildsまたはdiscord_list_channelsの結果から取得）。"
                        },
                        "channel_name": {
                            "type": "string",
                            "description": "チャンネル名（任意・ログ表示用のみ。省略可）。"
                        },
                        "readable": {
                            "type": "boolean",
                            "description": "このチャンネルのメッセージを読み取るか。falseにするとbotはそのチャンネルのメッセージを完全に無視する。"
                        },
                        "writable": {
                            "type": "boolean",
                            "description": "このチャンネルに返信・投稿するか。falseにするとbotはそのチャンネルへの送信を行わない。"
                        },
                        "whitelisted": {
                            "type": "boolean",
                            "description": "このチャンネルをホワイトリストに登録するか（trueにすると優先的に処理される）。デフォルトはfalse。"
                        }
                    },
                    "required": ["channel_id", "guild_id", "readable", "writable"]
                }),
            },
            GatewayActionDef {
                name: "discord_add_reaction".to_string(),
                description: "Discordメッセージにリアクション（絵文字）を追加する。Unicode絵文字（例: ⚡）またはカスタム絵文字（name:id形式）を指定できる。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "メッセージが存在するチャンネルの数値ID。"
                        },
                        "message_id": {
                            "type": "string",
                            "description": "リアクションを付けるメッセージの数値ID。現在処理中のメッセージのIDを使う場合はコンテキストから取得すること。"
                        },
                        "emoji": {
                            "type": "string",
                            "description": "Unicode絵文字（例: ⚡、👍）またはカスタム絵文字（形式: 絵文字名:数値ID、例: parrot:123456789012345）。Unicode絵文字の場合はそのまま文字列を渡す。"
                        }
                    },
                    "required": ["channel_id", "message_id", "emoji"]
                }),
            },
            GatewayActionDef {
                name: "list_duplicate_skills".to_string(),
                description: "エージェントの重複スキル（同名のスキルが複数存在するもの）を一覧表示する。マージ対象の候補を確認するために使用する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "merge_skills".to_string(),
                description: "2つのスキルをマージする。source_idのスキルをtarget_idのスキルに統合し、使用回数を合算してsourceをアーカイブする。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "source_id": {
                            "type": "string",
                            "description": "マージ元（削除される側）のスキルID"
                        },
                        "target_id": {
                            "type": "string",
                            "description": "マージ先（残る側）のスキルID"
                        }
                    },
                    "required": ["source_id", "target_id"]
                }),
            },
            GatewayActionDef {
                name: "update_memory_index_config".to_string(),
                description: "メモリインデックスの設定（batch_size、threshold）を更新する。少なくとも1つのパラメータを指定する必要がある。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "batch_size": {
                            "type": "integer",
                            "description": "一度に処理するメモリのバッチサイズ"
                        },
                        "threshold": {
                            "type": "integer",
                            "description": "インデックス再構築の閾値"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "add_allowed_command".to_string(),
                description: "シェルツールの許可コマンドリストに新しいコマンドを追加する。オーナーのみ実行可能。コマンド名（例: curl, wget, git）を指定する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "追加するコマンド名（英数字・ハイフン・アンダースコアのみ。例: curl, wget, git）"
                        }
                    },
                    "required": ["command"]
                }),
            },
            GatewayActionDef {
                name: "list_allowed_commands".to_string(),
                description: "現在DBに保存されている許可コマンドの一覧を取得する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "remove_allowed_command".to_string(),
                description: "シェルツールの許可コマンドリストからコマンドを削除する。オーナーのみ実行可能。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "削除するコマンド名"
                        }
                    },
                    "required": ["command"]
                }),
            },
            GatewayActionDef {
                name: "rebuild_memory_index".to_string(),
                description: "メモリインデックスをゼロから再構築する。既存のインデックスを削除し、全ログを再インデックスする。時間がかかることがある。結果として logs_indexed（処理したログ数）と nodes_created（作成したインデックスノード数）を返す。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "create_skill".to_string(),
                description: "ユーザーから「〇〇するスキルを作って」と言われたとき新しいスキルを作成する。guidanceにコマンド例・使い方を書くことで、LLMがexecute_shellで動的に実行できるようになる。同名スキルが存在する場合は更新される。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "スキル名"
                        },
                        "description": {
                            "type": "string",
                            "description": "スキルの説明"
                        },
                        "guidance": {
                            "type": "string",
                            "description": "スキルのガイダンス（省略時は空文字列）"
                        }
                    },
                    "required": ["name", "description"]
                }),
            },
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> GatewayActionResult {
        match name {
            "discord_list_guilds" => self.execute_list_guilds().await,
            "discord_list_channels" => self.execute_list_channels(args).await,
            "discord_channel_config" => self.execute_channel_config(args),
            "discord_add_reaction" => self.execute_add_reaction(args).await,
            "list_duplicate_skills" => self.execute_list_duplicate_skills(),
            "merge_skills" => self.execute_merge_skills(args),
            "update_memory_index_config" => self.execute_update_memory_index_config(args),
            "add_allowed_command" => self.execute_add_allowed_command(args),
            "list_allowed_commands" => self.execute_list_allowed_commands(),
            "remove_allowed_command" => self.execute_remove_allowed_command(args),
            "rebuild_memory_index" => self.execute_rebuild_memory_index().await,
            "create_skill" => self.execute_create_skill(args),
            _ => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// テスト用: serenity Httpは不要だがDiscordGatewayActionsの構築に必要。
    /// channel_config系テストではHTTP呼び出しは発生しないのでダミーでOK。
    fn make_test_actions() -> (DiscordGatewayActions, Arc<Mutex<rusqlite::Connection>>) {
        let db = Arc::new(Mutex::new(opencrab_db::init_memory().unwrap()));
        // serenityのHttpはダミートークンで作成（API呼び出しはしない）
        let http = Arc::new(Http::new("dummy-token"));
        let tools_config = Arc::new(std::sync::RwLock::new(opencrab_actions::tools::ToolsConfig::default()));
        let actions = DiscordGatewayActions::new(http, db.clone(), "test-agent".to_string(), tools_config, None, String::new());
        (actions, db)
    }

    // ---- definitions ----

    #[test]
    fn test_definitions_returns_four_actions() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 12);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
        assert!(names.contains(&"discord_add_reaction"));
        assert!(names.contains(&"list_duplicate_skills"));
        assert!(names.contains(&"merge_skills"));
        assert!(names.contains(&"update_memory_index_config"));
        assert!(names.contains(&"add_allowed_command"));
        assert!(names.contains(&"list_allowed_commands"));
        assert!(names.contains(&"remove_allowed_command"));
        assert!(names.contains(&"rebuild_memory_index"));
        assert!(names.contains(&"create_skill"));
    }

    #[test]
    fn test_definitions_have_valid_parameters() {
        let (actions, _db) = make_test_actions();
        for def in actions.definitions() {
            assert!(def.parameters.is_object(), "parameters should be object for {}", def.name);
            assert!(def.parameters["type"] == "object");
        }
    }

    // ---- channel_config ----

    #[tokio::test]
    async fn test_channel_config_upsert() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": false,
                }),
            )
            .await;
        assert!(result.success);
        assert!(result.error.is_none());

        let data = result.data.unwrap();
        assert_eq!(data["channel_id"], "ch-1");
        assert_eq!(data["readable"], true);
        assert_eq!(data["writable"], false);

        // DB確認
        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
            .unwrap()
            .unwrap();
        assert!(cfg.readable);
        assert!(!cfg.writable);
        assert_eq!(cfg.channel_name, "general");
        assert_eq!(cfg.guild_id, "guild-1");
    }

    #[tokio::test]
    async fn test_channel_config_update_existing() {
        let (actions, db) = make_test_actions();

        // 初回設定
        actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": true,
                }),
            )
            .await;

        // 更新
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": false,
                    "writable": false,
                }),
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
            .unwrap()
            .unwrap();
        assert!(!cfg.readable);
        assert!(!cfg.writable);
    }

    #[tokio::test]
    async fn test_channel_config_missing_params() {
        let (actions, _db) = make_test_actions();

        // channel_idのみ → guild_idが欠けてエラー
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({"channel_id": "ch-1"}),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_channel_config_missing_readable() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "writable": true,
                }),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("readable"));
    }

    #[tokio::test]
    async fn test_channel_config_optional_name() {
        let (actions, db) = make_test_actions();
        // channel_nameなしでも動く
        let result = actions
            .execute(
                "discord_channel_config",
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "readable": true,
                    "writable": true,
                }),
            )
            .await;
        assert!(result.success);

        let conn = db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
            .unwrap()
            .unwrap();
        assert_eq!(cfg.channel_name, "");
    }

    // ---- unknown action ----

    #[tokio::test]
    async fn test_unknown_gateway_action() {
        let (actions, _db) = make_test_actions();
        let result = actions.execute("nonexistent", &json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown gateway action"));
    }

    // ---- list_channels パラメータバリデーション ----

    #[tokio::test]
    async fn test_list_channels_missing_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute("discord_list_channels", &json!({}))
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    #[tokio::test]
    async fn test_list_channels_invalid_guild_id() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute("discord_list_channels", &json!({"guild_id": "not-a-number"}))
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("数値ID"));
    }

    // ---- create_skill ----

    #[tokio::test]
    async fn test_create_skill_basic() {
        let (actions, _db) = make_test_actions();
        let result = actions.execute("create_skill", &json!({
            "__caller": "owner",
            "name": "天気確認",
            "description": "curl wttr.inで天気を確認する"
        })).await;
        assert!(result.success, "create_skill should succeed");
        let data = result.data.unwrap();
        assert!(data["id"].is_string(), "should return id");
    }

    #[tokio::test]
    async fn test_create_skill_dedup() {
        let (actions, _db) = make_test_actions();
        // Create skill twice
        actions.execute("create_skill", &json!({
            "__caller": "owner",
            "name": "天気確認",
            "description": "first version"
        })).await;
        let result2 = actions.execute("create_skill", &json!({
            "__caller": "owner",
            "name": "天気確認",
            "description": "updated version"
        })).await;
        assert!(result2.success, "second create should succeed (dedup)");
    }

    #[tokio::test]
    async fn test_create_skill_rejected_for_non_owner() {
        let (actions, _db) = make_test_actions();
        let result = actions.execute("create_skill", &json!({
            "name": "test",
            "description": "test"
        })).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("trusted user"));
    }
}
