//! GatewayAdminトレイトのserenity実装
//!
//! `discord` featureが有効な場合のみコンパイルされる。

use std::sync::Arc;

use async_trait::async_trait;
use serenity::http::Http;
use serenity::model::prelude::ChannelType;
use tracing::{debug, error};

use opencrab_actions::traits::{ChannelInfo, GatewayAdmin, GuildInfo};

/// serenityのHTTPクライアントをラップしたGatewayAdmin実装
pub struct SerenityGatewayAdmin {
    http: Arc<Http>,
}

impl SerenityGatewayAdmin {
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }
}

#[async_trait]
impl GatewayAdmin for SerenityGatewayAdmin {
    async fn list_guilds(&self) -> anyhow::Result<Vec<GuildInfo>> {
        let guilds = self
            .http
            .get_guilds(None, None)
            .await
            .map_err(|e| {
                error!("Discord API get_guilds failed: {e}");
                anyhow::anyhow!("Failed to get guilds: {e}")
            })?;

        debug!("Got {} guilds from Discord API", guilds.len());

        Ok(guilds
            .into_iter()
            .map(|g| GuildInfo {
                id: g.id.to_string(),
                name: g.name,
                member_count: None,
            })
            .collect())
    }

    async fn list_channels(&self, guild_id: &str) -> anyhow::Result<Vec<ChannelInfo>> {
        let gid: u64 = guild_id.parse().map_err(|_| {
            error!("Invalid guild_id passed to list_channels: {guild_id}");
            anyhow::anyhow!(
                "guild_idが数値IDではありません: '{guild_id}' — guild名ではなくdiscord_list_guildsで取得したIDを使ってください"
            )
        })?;

        let channels = self
            .http
            .get_channels(serenity::model::id::GuildId::new(gid))
            .await
            .map_err(|e| {
                error!("Discord API get_channels failed for guild {gid}: {e}");
                anyhow::anyhow!("Failed to get channels for guild {gid}: {e}")
            })?;

        debug!(
            "Got {} channels from guild {gid}, {} are text",
            channels.len(),
            channels.iter().filter(|c| c.kind == ChannelType::Text).count()
        );

        Ok(channels
            .into_iter()
            .filter(|ch| ch.kind == ChannelType::Text)
            .map(|ch| ChannelInfo {
                id: ch.id.to_string(),
                name: ch.name,
                kind: "text".to_string(),
            })
            .collect())
    }
}
