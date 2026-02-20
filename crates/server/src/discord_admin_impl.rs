//! DiscordゲートウェイアクションのGatewayActions実装
//!
//! `discord` featureが有効な場合のみコンパイルされる。
//! Discord管理操作（サーバー一覧、チャンネル一覧、チャンネル設定）を
//! ゲートウェイ固有アクションとして提供する。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::prelude::ChannelType;
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
}

impl DiscordGatewayActions {
    pub fn new(http: Arc<Http>, db: Arc<Mutex<rusqlite::Connection>>) -> Self {
        Self { http, db }
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

                json!({
                    "id": ch_id,
                    "name": ch.name,
                    "kind": "text",
                    "readable": readable,
                    "writable": writable,
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

        let cfg = opencrab_db::queries::ChannelConfigRow {
            channel_id: channel_id.to_string(),
            guild_id: guild_id.to_string(),
            channel_name: channel_name.to_string(),
            readable,
            writable,
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
}

#[async_trait]
impl GatewayActions for DiscordGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "discord_list_guilds".to_string(),
                description: "Botが参加しているDiscordサーバー（guild）の一覧を取得する".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "discord_list_channels".to_string(),
                description: "指定サーバーのチャンネル一覧と、現在のread/write設定を取得する。guild_idはdiscord_list_guildsで取得した数値IDを使うこと。".to_string(),
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
                description: "Discordチャンネルの読み書き設定を変更する。readableをfalseにするとそのチャンネルのメッセージを無視し、writableをfalseにすると返信しない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "channel_id": {
                            "type": "string",
                            "description": "対象チャンネルのID"
                        },
                        "guild_id": {
                            "type": "string",
                            "description": "チャンネルが属するサーバーの数値ID"
                        },
                        "channel_name": {
                            "type": "string",
                            "description": "チャンネル名（表示用）"
                        },
                        "readable": {
                            "type": "boolean",
                            "description": "このチャンネルのメッセージを読むか"
                        },
                        "writable": {
                            "type": "boolean",
                            "description": "このチャンネルに返信するか"
                        }
                    },
                    "required": ["channel_id", "guild_id", "readable", "writable"]
                }),
            },
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> GatewayActionResult {
        match name {
            "discord_list_guilds" => self.execute_list_guilds().await,
            "discord_list_channels" => self.execute_list_channels(args).await,
            "discord_channel_config" => self.execute_channel_config(args),
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
        let actions = DiscordGatewayActions::new(http, db.clone());
        (actions, db)
    }

    // ---- definitions ----

    #[test]
    fn test_definitions_returns_three_actions() {
        let (actions, _db) = make_test_actions();
        let defs = actions.definitions();
        assert_eq!(defs.len(), 3);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"discord_list_guilds"));
        assert!(names.contains(&"discord_list_channels"));
        assert!(names.contains(&"discord_channel_config"));
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
}
