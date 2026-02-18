//! Discord管理アクション
//!
//! エージェントがDiscordのサーバー・チャンネルを把握・制御するためのアクション群。
//! LLMが自然言語の指示に基づいて適切なツールを選択する。

use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

// ============================================
// discord_list_guilds: サーバー一覧取得
// ============================================

pub struct DiscordListGuildsAction;

#[async_trait]
impl Action for DiscordListGuildsAction {
    fn name(&self) -> &str {
        "discord_list_guilds"
    }

    fn description(&self) -> &str {
        "Botが参加しているDiscordサーバー（guild）の一覧を取得する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        ctx: &ActionContext,
    ) -> ActionResult {
        let admin = match &ctx.gateway_admin {
            Some(a) => a,
            None => return ActionResult::error("Discord未接続: gateway_adminが設定されていません"),
        };

        match admin.list_guilds().await {
            Ok(guilds) => {
                let guild_list: Vec<serde_json::Value> = guilds
                    .iter()
                    .map(|g| {
                        json!({
                            "id": g.id,
                            "name": g.name,
                            "member_count": g.member_count,
                        })
                    })
                    .collect();
                ActionResult::success(json!({
                    "guilds": guild_list,
                    "count": guild_list.len(),
                }))
            }
            Err(e) => ActionResult::error(&format!("サーバー一覧の取得に失敗: {e}")),
        }
    }
}

// ============================================
// discord_list_channels: チャンネル一覧取得
// ============================================

pub struct DiscordListChannelsAction;

#[async_trait]
impl Action for DiscordListChannelsAction {
    fn name(&self) -> &str {
        "discord_list_channels"
    }

    fn description(&self) -> &str {
        "指定サーバーのチャンネル一覧と、現在のread/write設定を取得する。guild_idはdiscord_list_guildsで取得した数値IDを使うこと。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "guild_id": {
                    "type": "string",
                    "description": "対象サーバーの数値ID（discord_list_guildsの結果から取得）。サーバー名ではなくIDを指定すること。"
                }
            },
            "required": ["guild_id"]
        })
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        ctx: &ActionContext,
    ) -> ActionResult {
        let guild_id = match args.get("guild_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ActionResult::error("guild_idパラメータが必要です"),
        };

        let admin = match &ctx.gateway_admin {
            Some(a) => a,
            None => return ActionResult::error("Discord未接続: gateway_adminが設定されていません"),
        };

        let channels = match admin.list_channels(guild_id).await {
            Ok(chs) => chs,
            Err(e) => return ActionResult::error(&format!("チャンネル一覧の取得に失敗: {e}")),
        };

        // DB設定も合わせて取得
        let db_configs = {
            let conn = ctx.db.lock().unwrap();
            opencrab_db::queries::list_channel_configs_by_guild(&conn, guild_id)
                .unwrap_or_default()
        };

        let channel_list: Vec<serde_json::Value> = channels
            .iter()
            .map(|ch| {
                let db_cfg = db_configs.iter().find(|c| c.channel_id == ch.id);
                let readable = db_cfg.map(|c| c.readable).unwrap_or(true);
                let writable = db_cfg.map(|c| c.writable).unwrap_or(true);

                json!({
                    "id": ch.id,
                    "name": ch.name,
                    "kind": ch.kind,
                    "readable": readable,
                    "writable": writable,
                })
            })
            .collect();

        ActionResult::success(json!({
            "guild_id": guild_id,
            "channels": channel_list,
            "count": channel_list.len(),
        }))
    }
}

// ============================================
// discord_channel_config: チャンネル設定変更
// ============================================

pub struct DiscordChannelConfigAction;

#[async_trait]
impl Action for DiscordChannelConfigAction {
    fn name(&self) -> &str {
        "discord_channel_config"
    }

    fn description(&self) -> &str {
        "Discordチャンネルの読み書き設定を変更する。readableをfalseにするとそのチャンネルのメッセージを無視し、writableをfalseにすると返信しない。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
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
        })
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        ctx: &ActionContext,
    ) -> ActionResult {
        let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ActionResult::error("channel_idパラメータが必要です"),
        };
        let guild_id = match args.get("guild_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ActionResult::error("guild_idパラメータが必要です"),
        };
        let channel_name = args
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let readable = match args.get("readable").and_then(|v| v.as_bool()) {
            Some(r) => r,
            None => return ActionResult::error("readableパラメータが必要です"),
        };
        let writable = match args.get("writable").and_then(|v| v.as_bool()) {
            Some(w) => w,
            None => return ActionResult::error("writableパラメータが必要です"),
        };

        let cfg = opencrab_db::queries::ChannelConfigRow {
            channel_id: channel_id.to_string(),
            guild_id: guild_id.to_string(),
            channel_name: channel_name.to_string(),
            readable,
            writable,
        };

        let result = {
            let conn = ctx.db.lock().unwrap();
            opencrab_db::queries::upsert_channel_config(&conn, &cfg)
        };

        match result {
            Ok(()) => ActionResult::success(json!({
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
            Err(e) => ActionResult::error(&format!("チャンネル設定の保存に失敗: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{ChannelInfo, GatewayAdmin, GuildInfo};
    use serde_json::json;
    use std::sync::Arc;

    /// テスト用のGatewayAdminモック
    struct MockGatewayAdmin {
        guilds: Vec<GuildInfo>,
        channels: Vec<ChannelInfo>,
    }

    #[async_trait]
    impl GatewayAdmin for MockGatewayAdmin {
        async fn list_guilds(&self) -> anyhow::Result<Vec<GuildInfo>> {
            Ok(self.guilds.clone())
        }

        async fn list_channels(&self, _guild_id: &str) -> anyhow::Result<Vec<ChannelInfo>> {
            Ok(self.channels.clone())
        }
    }

    /// エラーを返すGatewayAdminモック
    struct FailingGatewayAdmin;

    #[async_trait]
    impl GatewayAdmin for FailingGatewayAdmin {
        async fn list_guilds(&self) -> anyhow::Result<Vec<GuildInfo>> {
            Err(anyhow::anyhow!("API rate limited"))
        }

        async fn list_channels(&self, _guild_id: &str) -> anyhow::Result<Vec<ChannelInfo>> {
            Err(anyhow::anyhow!("Guild not found"))
        }
    }

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: std::sync::Arc::new(std::sync::Mutex::new(conn)),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            gateway_admin: None,
        };
        (dir, ctx)
    }

    fn test_context_with_mock(admin: impl GatewayAdmin + 'static) -> (tempfile::TempDir, ActionContext) {
        let (dir, mut ctx) = test_context();
        ctx.gateway_admin = Some(Arc::new(admin));
        (dir, ctx)
    }

    // ---- gateway_admin = None ----

    #[tokio::test]
    async fn test_list_guilds_no_gateway() {
        let (_dir, ctx) = test_context();
        let result = DiscordListGuildsAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Discord未接続"));
    }

    #[tokio::test]
    async fn test_list_channels_no_gateway() {
        let (_dir, ctx) = test_context();
        let result = DiscordListChannelsAction
            .execute(&json!({"guild_id": "123"}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Discord未接続"));
    }

    // ---- gateway_admin = Some (mock) ----

    #[tokio::test]
    async fn test_list_guilds_returns_data() {
        let mock = MockGatewayAdmin {
            guilds: vec![
                GuildInfo {
                    id: "111".to_string(),
                    name: "Test Server".to_string(),
                    member_count: Some(42),
                },
                GuildInfo {
                    id: "222".to_string(),
                    name: "Dev Server".to_string(),
                    member_count: None,
                },
            ],
            channels: vec![],
        };
        let (_dir, ctx) = test_context_with_mock(mock);

        let result = DiscordListGuildsAction.execute(&json!({}), &ctx).await;
        assert!(result.success);

        let data = result.data.unwrap();
        assert_eq!(data["count"], 2);

        let guilds = data["guilds"].as_array().unwrap();
        assert_eq!(guilds[0]["id"], "111");
        assert_eq!(guilds[0]["name"], "Test Server");
        assert_eq!(guilds[0]["member_count"], 42);
        assert_eq!(guilds[1]["id"], "222");
        assert_eq!(guilds[1]["name"], "Dev Server");
        assert!(guilds[1]["member_count"].is_null());
    }

    #[tokio::test]
    async fn test_list_guilds_empty() {
        let mock = MockGatewayAdmin {
            guilds: vec![],
            channels: vec![],
        };
        let (_dir, ctx) = test_context_with_mock(mock);

        let result = DiscordListGuildsAction.execute(&json!({}), &ctx).await;
        assert!(result.success);

        let data = result.data.unwrap();
        assert_eq!(data["count"], 0);
        assert!(data["guilds"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_channels_returns_data() {
        let mock = MockGatewayAdmin {
            guilds: vec![],
            channels: vec![
                ChannelInfo {
                    id: "ch-100".to_string(),
                    name: "general".to_string(),
                    kind: "text".to_string(),
                },
                ChannelInfo {
                    id: "ch-200".to_string(),
                    name: "random".to_string(),
                    kind: "text".to_string(),
                },
            ],
        };
        let (_dir, ctx) = test_context_with_mock(mock);

        let result = DiscordListChannelsAction
            .execute(&json!({"guild_id": "111"}), &ctx)
            .await;
        assert!(result.success);

        let data = result.data.unwrap();
        assert_eq!(data["count"], 2);
        assert_eq!(data["guild_id"], "111");

        let channels = data["channels"].as_array().unwrap();
        assert_eq!(channels[0]["id"], "ch-100");
        assert_eq!(channels[0]["name"], "general");
        // デフォルトはread/write=true
        assert_eq!(channels[0]["readable"], true);
        assert_eq!(channels[0]["writable"], true);
    }

    #[tokio::test]
    async fn test_list_channels_reflects_db_config() {
        let mock = MockGatewayAdmin {
            guilds: vec![],
            channels: vec![
                ChannelInfo {
                    id: "ch-100".to_string(),
                    name: "general".to_string(),
                    kind: "text".to_string(),
                },
                ChannelInfo {
                    id: "ch-200".to_string(),
                    name: "random".to_string(),
                    kind: "text".to_string(),
                },
            ],
        };
        let (_dir, ctx) = test_context_with_mock(mock);

        // ch-100のwritableをfalseに設定
        {
            let conn = ctx.db.lock().unwrap();
            opencrab_db::queries::upsert_channel_config(
                &conn,
                &opencrab_db::queries::ChannelConfigRow {
                    channel_id: "ch-100".to_string(),
                    guild_id: "111".to_string(),
                    channel_name: "general".to_string(),
                    readable: true,
                    writable: false,
                },
            )
            .unwrap();
        }

        let result = DiscordListChannelsAction
            .execute(&json!({"guild_id": "111"}), &ctx)
            .await;
        assert!(result.success);

        let channels = result.data.unwrap()["channels"].as_array().unwrap().clone();
        // ch-100: writable=false（DB設定反映）
        let general = channels.iter().find(|c| c["id"] == "ch-100").unwrap();
        assert_eq!(general["readable"], true);
        assert_eq!(general["writable"], false);
        // ch-200: DB設定なし→デフォルトtrue
        let random = channels.iter().find(|c| c["id"] == "ch-200").unwrap();
        assert_eq!(random["readable"], true);
        assert_eq!(random["writable"], true);
    }

    #[tokio::test]
    async fn test_list_channels_missing_guild_id() {
        let mock = MockGatewayAdmin {
            guilds: vec![],
            channels: vec![],
        };
        let (_dir, ctx) = test_context_with_mock(mock);

        let result = DiscordListChannelsAction
            .execute(&json!({}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("guild_id"));
    }

    // ---- gateway API error ----

    #[tokio::test]
    async fn test_list_guilds_api_error() {
        let (_dir, ctx) = test_context_with_mock(FailingGatewayAdmin);

        let result = DiscordListGuildsAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("API rate limited"));
    }

    #[tokio::test]
    async fn test_list_channels_api_error() {
        let (_dir, ctx) = test_context_with_mock(FailingGatewayAdmin);

        let result = DiscordListChannelsAction
            .execute(&json!({"guild_id": "999"}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Guild not found"));
    }

    // ---- channel_config action ----

    #[tokio::test]
    async fn test_channel_config_upsert() {
        let (_dir, ctx) = test_context();
        let result = DiscordChannelConfigAction
            .execute(
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": false,
                }),
                &ctx,
            )
            .await;
        assert!(result.success);

        // Verify DB state
        let conn = ctx.db.lock().unwrap();
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
        let (_dir, ctx) = test_context();

        // 初回設定
        DiscordChannelConfigAction
            .execute(
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": true,
                    "writable": true,
                }),
                &ctx,
            )
            .await;

        // 更新
        let result = DiscordChannelConfigAction
            .execute(
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "channel_name": "general",
                    "readable": false,
                    "writable": false,
                }),
                &ctx,
            )
            .await;
        assert!(result.success);

        let conn = ctx.db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
            .unwrap()
            .unwrap();
        assert!(!cfg.readable);
        assert!(!cfg.writable);
    }

    #[tokio::test]
    async fn test_channel_config_missing_params() {
        let (_dir, ctx) = test_context();
        let result = DiscordChannelConfigAction
            .execute(&json!({"channel_id": "ch-1"}), &ctx)
            .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_channel_config_optional_name() {
        let (_dir, ctx) = test_context();
        // channel_nameなしでも動く
        let result = DiscordChannelConfigAction
            .execute(
                &json!({
                    "channel_id": "ch-1",
                    "guild_id": "guild-1",
                    "readable": true,
                    "writable": true,
                }),
                &ctx,
            )
            .await;
        assert!(result.success);

        let conn = ctx.db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config(&conn, "ch-1")
            .unwrap()
            .unwrap();
        assert_eq!(cfg.channel_name, "");
    }
}
