//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! Discord管理操作（サーバー一覧、チャンネル一覧、チャンネル設定）を
//! ゲートウェイ固有アクションとして提供する。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use dashmap::DashMap;
use serenity::all::{ChannelId, CreateAttachment, CreateMessage};
use serenity::http::Http;
use serenity::model::prelude::ChannelType;
use serenity::model::id::MessageId;
use serenity::model::channel::ReactionType;
use serde_json::json;
use tokio::task::AbortHandle;
use tracing::{debug, error};
use uuid::Uuid;
use opencrab_gateway::{GatewayActions, GatewayActionDef, GatewayActionResult};

/// A running subtask tracked by the registry.
pub struct SpawnedSubtask {
    pub abort_handle: AbortHandle,
    pub session_id: String,
    pub parent_session_id: String,
    pub spawned_at: String,
    pub agent_id: String,
}

/// Callback invoked when a subtask completes.
/// Args: (subtask_id: String, result: String, exit_reason: String)
pub type SubtaskCompletionFn = Arc<dyn Fn(String, String, String) + Send + Sync>;

/// Registry of completion callbacks keyed by parent_session_id.
pub type CompletionRegistry = Arc<DashMap<String, SubtaskCompletionFn>>;

/// Registry of active subtasks keyed by subtask_id.
pub type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>;

struct ArcLlmClient(Arc<dyn opencrab_core::LlmClient>);

#[async_trait::async_trait]
impl opencrab_core::LlmClient for ArcLlmClient {
    async fn chat(&self, request: opencrab_core::ChatRequestSimple) -> anyhow::Result<opencrab_core::ChatResponseSimple> {
        self.0.chat(request).await
    }
}

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
    workspace_root: PathBuf,
    subtask_registry: SubtaskRegistry,
    completion_registry: CompletionRegistry,
}

impl DiscordGatewayActions {
    pub fn new(
        http: Arc<Http>,
        db: Arc<Mutex<rusqlite::Connection>>,
        agent_id: String,
        tools_config: Arc<std::sync::RwLock<opencrab_actions::tools::ToolsConfig>>,
        llm_client: Option<Arc<dyn opencrab_core::LlmClient>>,
        default_model: String,
        workspace_root: PathBuf,
        subtask_registry: SubtaskRegistry,
        completion_registry: CompletionRegistry,
    ) -> Self {
        Self { http, db, agent_id, tools_config, llm_client, default_model, workspace_root, subtask_registry, completion_registry }
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

    async fn execute_send_file(&self, args: &serde_json::Value) -> GatewayActionResult {
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
                error: Some(format!("ワークスペース外のファイルは送信できません: {file_path_str}")),
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
        match ChannelId::new(channel_id).send_message(&self.http, msg).await {
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

    async fn execute_spawn_subtask(&self, args: &serde_json::Value) -> GatewayActionResult {
        let task = match args["task"].as_str() {
            Some(t) => t.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("spawn_subtask: 'task' argument is required".to_string()),
            },
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(1800) as u64;
        let parent_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let parent_depth = args["__depth"].as_u64().unwrap_or(0) as u32;
        let agent_id = args["__agent_id"].as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        let subtask_id = Uuid::new_v4().to_string();
        let sub_session_id = format!("subtask-{}", subtask_id);
        let spawned_at = Utc::now().to_rfc3339();
        let depth = parent_depth + 1;

        // Create the sub-session in the DB.
        {
            let conn = self.db.lock().unwrap();
            let meta = serde_json::json!({
                "parent_session_id": parent_session_id,
                "depth": depth,
                "subtask_id": subtask_id,
            });
            let session = opencrab_db::queries::SessionRow {
                id: sub_session_id.clone(),
                mode: "subtask".to_string(),
                theme: format!("Subtask: {}", &task.chars().take(50).collect::<String>()),
                phase: "active".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!([&agent_id]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: Some(meta.to_string()),
            };
            opencrab_db::queries::insert_session(&conn, &session).ok();

            // Write subtask_spawned to parent session log.
            if !parent_session_id.is_empty() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: serde_json::json!({
                        "type": "subtask_spawned",
                        "subtask_id": subtask_id,
                        "session_id": sub_session_id,
                        "spawned_at": spawned_at,
                    }).to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }

        // Build sub-engine context.
        let llm_client = match self.llm_client.clone() {
            Some(c) => c,
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("spawn_subtask: no LLM client available".to_string()),
            },
        };

        let ws_path = self.workspace_root.join(&agent_id);
        std::fs::create_dir_all(&ws_path).ok();
        let workspace = match opencrab_core::workspace::Workspace::from_root(&ws_path) {
            Ok(w) => w,
            Err(e) => return GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("spawn_subtask: workspace error: {e}")),
            },
        };

        let sub_ctx = opencrab_actions::ActionContext {
            caller: opencrab_actions::CallerIdentity::Agent,
            agent_id: agent_id.clone(),
            agent_name: agent_id.clone(),
            session_id: Some(sub_session_id.clone()),
            db: self.db.clone(),
            workspace: Arc::new(workspace),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            model_override: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("subtask".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
                default_model: self.default_model.clone(),
                active_model: None,
                available_providers: vec![],
                gateway: "subtask".to_string(),
            })),
        };

        let mut sub_dispatcher = opencrab_actions::ActionDispatcher::new();
        let tools_cfg = self.tools_config.read().unwrap().clone();
        opencrab_actions::register_tools_from_config(&tools_cfg, &mut sub_dispatcher);
        let sub_executor = opencrab_actions::BridgedExecutor::new(sub_dispatcher, sub_ctx)
            .with_depth(depth);

        let sub_engine = opencrab_core::SkillEngine::new(
            Box::new(ArcLlmClient(llm_client)),
            Box::new(sub_executor),
            usize::MAX,
        );

        // System prompt for the sub-engine.
        let sub_system_prompt = format!(
            "あなたはサブエンジンとして起動されています。\n\
             - subtask_id: {subtask_id}\n\
             - depth: {depth}\n\
             - Discordへの直接送信は禁止されています\n\
             - 進捗報告は report_progress を使ってください\n\
             - タスク完了時はテキストで結果を返してください（Discord送信はメインエンジンが行います）\n\n\
             You are a sub-engine executing a delegated task."
        );

        // Clone for the spawned task.
        let db_clone = self.db.clone();
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let sub_session_id_clone = sub_session_id.clone();
        let agent_id_clone = agent_id.clone();
        let completion_registry_clone = self.completion_registry.clone();
        let subtask_registry_clone = self.subtask_registry.clone();
        let default_model_clone = self.default_model.clone();

        let join_handle = tokio::spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                sub_engine.run_with_model_override(
                    &sub_system_prompt,
                    &task,
                    &default_model_clone,
                    None,
                    &[],
                ),
            )
            .await;

            let (exit_reason, result_text) = match result {
                Ok(Ok(engine_result)) => {
                    let exit_reason = if engine_result.stopped_by_limit {
                        "stopped_by_limit"
                    } else {
                        "completed"
                    };
                    (exit_reason.to_string(), engine_result.response)
                }
                Ok(Err(e)) => ("error".to_string(), format!("Error: {e}")),
                Err(_) => ("timeout".to_string(), "Subtask timed out.".to_string()),
            };

            // Write subtask_completed to parent session log.
            if !parent_session_clone.is_empty() {
                if let Ok(conn) = db_clone.lock() {
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id_clone.clone(),
                        session_id: parent_session_clone.clone(),
                        log_type: "system".to_string(),
                        content: serde_json::json!({
                            "type": "subtask_completed",
                            "subtask_id": subtask_id_clone,
                            "session_id": sub_session_id_clone,
                            "exit_reason": exit_reason,
                            "result": result_text,
                        }).to_string(),
                        speaker_id: None,
                        turn_number: None,
                        metadata_json: None,
                    };
                    opencrab_db::queries::insert_session_log(&conn, &log).ok();
                }
            }

            // Remove from registry.
            subtask_registry_clone.remove(&subtask_id_clone);

            // Call completion callback if registered.
            if let Some(cb) = completion_registry_clone.get(&parent_session_clone) {
                cb(subtask_id_clone.clone(), result_text.clone(), exit_reason.clone());
            }
        });

        let abort_handle = join_handle.abort_handle();
        self.subtask_registry.insert(subtask_id.clone(), SpawnedSubtask {
            abort_handle,
            session_id: sub_session_id.clone(),
            parent_session_id: parent_session_id.clone(),
            spawned_at: spawned_at.clone(),
            agent_id: agent_id.clone(),
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "status": "spawned",
                "subtask_id": subtask_id,
                "session_id": sub_session_id,
                "spawned_at": spawned_at,
            })),
            error: None,
        }
    }

    fn execute_cancel_subtask(&self, args: &serde_json::Value) -> GatewayActionResult {
        let subtask_id = match args["subtask_id"].as_str() {
            Some(id) => id.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("cancel_subtask: 'subtask_id' is required".to_string()),
            },
        };

        match self.subtask_registry.remove(&subtask_id) {
            Some((_, subtask)) => {
                subtask.abort_handle.abort();

                // Write subtask_cancelled to parent session log.
                let parent_session_id = subtask.parent_session_id.clone();
                if !parent_session_id.is_empty() {
                    if let Ok(conn) = self.db.lock() {
                        let log = opencrab_db::queries::SessionLogRow {
                            id: None,
                            agent_id: subtask.agent_id.clone(),
                            session_id: parent_session_id.clone(),
                            log_type: "system".to_string(),
                            content: serde_json::json!({
                                "type": "subtask_cancelled",
                                "subtask_id": subtask_id,
                            }).to_string(),
                            speaker_id: None,
                            turn_number: None,
                            metadata_json: None,
                        };
                        opencrab_db::queries::insert_session_log(&conn, &log).ok();
                    }
                }

                GatewayActionResult {
                    success: true,
                    data: Some(json!({"cancelled": true, "subtask_id": subtask_id})),
                    error: None,
                }
            }
            None => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("cancel_subtask: subtask '{}' not found", subtask_id)),
            },
        }
    }

    async fn execute_report_progress(&self, args: &serde_json::Value) -> GatewayActionResult {
        let message = match args["message"].as_str() {
            Some(m) => m.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("report_progress: 'message' is required".to_string()),
            },
        };
        let parent_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let subtask_id = args.get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = args["__agent_id"].as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        // Write progress to parent session log.
        if !parent_session_id.is_empty() {
            if let Ok(conn) = self.db.lock() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: serde_json::json!({
                        "type": "subtask_progress",
                        "subtask_id": subtask_id,
                        "message": message,
                        "timestamp": Utc::now().to_rfc3339(),
                    }).to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                };
                opencrab_db::queries::insert_session_log(&conn, &log).ok();
            }
        }

        // Debounce: wait 3 seconds then trigger main engine re-invocation via completion callback.
        let completion_registry_clone = self.completion_registry.clone();
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let message_clone = message.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Some(cb) = completion_registry_clone.get(&parent_session_clone) {
                cb(subtask_id_clone, message_clone, "progress".to_string());
            }
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({"reported": true, "message": message})),
            error: None,
        }
    }

    async fn execute_spawn_coding_agent(&self, args: &serde_json::Value) -> GatewayActionResult {
        let agent_type = args["agent_type"].as_str().unwrap_or("claude").to_string();
        let task = match args["task"].as_str() {
            Some(t) => t.to_string(),
            None => return GatewayActionResult {
                success: false,
                data: None,
                error: Some("spawn_coding_agent: 'task' is required".to_string()),
            },
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(1800);
        let _parent_session_id = args["__session_id"].as_str().unwrap_or("").to_string();
        let agent_id = args["__agent_id"].as_str()
            .unwrap_or(&self.agent_id)
            .to_string();

        let subtask_id = Uuid::new_v4().to_string();

        // Generate progress_report.sh in the workspace.
        let ws_path = self.workspace_root.join(&agent_id);
        std::fs::create_dir_all(&ws_path).ok();
        let progress_script = format!(
            "#!/bin/bash\n\
             MESSAGE=\"$1\"\n\
             curl -s -X POST http://localhost:8080/api/agents/{}/subtasks/{}/progress \\\n\
               -H \"Content-Type: application/json\" \\\n\
               -d \"{{\\\"message\\\": \\\"$MESSAGE\\\"}}\"\n",
            agent_id, subtask_id
        );
        let script_path = ws_path.join("progress_report.sh");
        if let Err(e) = std::fs::write(&script_path, &progress_script) {
            tracing::warn!("Failed to write progress_report.sh: {e}");
        } else {
            // Make executable
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
            }
        }

        // Delegate to spawn_subtask with coding agent system prompt.
        let enhanced_task = format!(
            "あなたはコーディングエージェント（{agent_type}）として起動されています。\n\
             各ステップ完了時は ./progress_report.sh 'メッセージ' を呼んで進捗を報告してください。\n\n\
             タスク:\n{task}"
        );

        let mut spawn_args = args.clone();
        if let serde_json::Value::Object(ref mut map) = spawn_args {
            map.insert("task".to_string(), serde_json::json!(enhanced_task));
            map.insert("timeout_secs".to_string(), serde_json::json!(timeout_secs));
        }

        self.execute_spawn_subtask(&spawn_args).await
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
            GatewayActionDef {
                name: "discord_send_file".to_string(),
                description: "Discordチャンネルにファイル（画像等）をアップロードして送信する。ファイルパスはワークスペース内のパスのみ指定可能（パストラバーサル防止）。25MBサイズ制限あり。".to_string(),
                parameters: json!({
                    "type": "object",
                    "required": ["channel_id", "file_path"],
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "送信先DiscordチャンネルのID（数値文字列）。現在のチャンネルIDはシステムプロンプトの[Discord context]セクションに`channel_id=XXXX`として記載されている。ユーザーIDやBotIDではなくチャンネルIDを指定すること。"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "送信するファイルのパス（ワークスペース相対パスまたは絶対パス）"
                        },
                        "caption": {
                            "type": "string",
                            "description": "ファイルに添付するテキストキャプション（省略可）"
                        },
                        "filename": {
                            "type": "string",
                            "description": "Discord上で表示されるファイル名（省略時は元のファイル名）"
                        }
                    }
                }),
            },
            GatewayActionDef {
                name: "spawn_subtask".to_string(),
                description: "バックグラウンドでサブタスクを起動します。LLMエンジンがサブエンジンとして非同期実行し、完了後にメインエンジンを自動的に再呼び出しします。複雑な長時間処理（画像生成・コード実装・調査など）に使用してください。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "サブエンジンに実行させるタスクの説明"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "タイムアウト秒数（省略時1800秒）"
                        },
                        "max_iterations": {
                            "type": "integer",
                            "description": "LLMループの最大イテレーション数（省略時は無制限）"
                        }
                    },
                    "required": ["task"]
                }),
            },
            GatewayActionDef {
                name: "cancel_subtask".to_string(),
                description: "実行中のサブタスクをキャンセルします。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "キャンセルするサブタスクのID（subtask_spawnedイベントから取得）"
                        }
                    },
                    "required": ["subtask_id"]
                }),
            },
            GatewayActionDef {
                name: "report_progress".to_string(),
                description: "サブエンジンからメインエンジンへ進捗を報告します。depth >= 1のサブエンジンのみ使用可能。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "進捗メッセージ"
                        },
                        "subtask_id": {
                            "type": "string",
                            "description": "このサブタスクのID（オプション）"
                        }
                    },
                    "required": ["message"]
                }),
            },
            GatewayActionDef {
                name: "spawn_coding_agent".to_string(),
                description: "コーディングエージェント（Claude Code/Codex）をサブタスクとして起動します。progress_report.shを自動生成し、進捗報告スクリプトをワークスペースに配置します。".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_type": {
                            "type": "string",
                            "enum": ["claude", "codex"],
                            "description": "コーディングエージェントの種類"
                        },
                        "task": {
                            "type": "string",
                            "description": "コーディングエージェントに実行させるタスク"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "タイムアウト秒数（省略時1800秒）"
                        }
                    },
                    "required": ["agent_type", "task"]
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
            "update_memory_index_config" => self.execute_update_memory_index_config(args),
            "add_allowed_command" => self.execute_add_allowed_command(args),
            "list_allowed_commands" => self.execute_list_allowed_commands(),
            "remove_allowed_command" => self.execute_remove_allowed_command(args),
            "rebuild_memory_index" => self.execute_rebuild_memory_index().await,
            "create_skill" => self.execute_create_skill(args),
            "discord_send_file" => self.execute_send_file(args).await,
            "spawn_subtask" => self.execute_spawn_subtask(args).await,
            "cancel_subtask" => self.execute_cancel_subtask(args),
            "report_progress" => self.execute_report_progress(args).await,
            "spawn_coding_agent" => self.execute_spawn_coding_agent(args).await,
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
        let subtask_registry: SubtaskRegistry = Arc::new(DashMap::new());
        let completion_registry: CompletionRegistry = Arc::new(DashMap::new());
        let actions = DiscordGatewayActions::new(http, db.clone(), "test-agent".to_string(), tools_config, None, String::new(), std::path::PathBuf::from("/tmp"), subtask_registry, completion_registry);
        (actions, db)
    }

    // ---- definitions ----

    #[test]
    fn test_definitions_returns_four_actions() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 15);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
        assert!(names.contains(&"discord_add_reaction"));
        assert!(names.contains(&"update_memory_index_config"));
        assert!(names.contains(&"add_allowed_command"));
        assert!(names.contains(&"list_allowed_commands"));
        assert!(names.contains(&"remove_allowed_command"));
        assert!(names.contains(&"rebuild_memory_index"));
        assert!(names.contains(&"create_skill"));
        assert!(names.contains(&"discord_send_file"));
        assert!(names.contains(&"spawn_subtask"));
        assert!(names.contains(&"cancel_subtask"));
        assert!(names.contains(&"report_progress"));
        assert!(names.contains(&"spawn_coding_agent"));
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

    // ---- discord_send_file ----

    #[tokio::test]
    async fn test_send_file_workspace_violation() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "discord_send_file",
                &json!({
                    "channel_id": "12345678901234567",
                    "file_path": "/etc/passwd",
                }),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("ワークスペース外") || result.error.as_ref().unwrap().contains("見つかりません"));
    }

    #[tokio::test]
    async fn test_send_file_missing_params() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute("discord_send_file", &json!({"channel_id": "123"}))
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("file_path"));
    }
}
