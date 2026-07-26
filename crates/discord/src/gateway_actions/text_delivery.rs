//! 素テキスト配送口の Discord 実装（#157 S7）。
//!
//! `request_peer_review` の実体（定義・引数検査・レビュアー解決・メッセージ組み立て・
//! 分割送信の勘定・台帳記録）は gateway 非依存層（`crates/server/src/peer_review.rs`）へ
//! 移設済み。ここに残るのは **transport 固有の 4 つだけ**:
//! 1. 宛先トークンの検査（Discord のチャンネル ID は数値スノーフレーク）
//! 2. メンションの記法（`<@id>`）
//! 3. 1 通に収める安全な文字数上限（[`DISCORD_CHUNK_LIMIT`]）
//! 4. 送信そのもの（serenity の直叩き）
//!
//! **分割の仕方と部分失敗の勘定（「N/M 通送信済み」）は汎用層に残してある**: 1 通ずつ
//! 送る境界にすることで、途中失敗の通数が抽象を越えても失われない。

use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::{ChannelId, CreateMessage};
use serenity::http::Http;

use opencrab_core::text_delivery::TextDelivery;

use super::webhook::DISCORD_CHUNK_LIMIT;
use super::DiscordGatewayActions;

/// Discord の素テキスト配送口。
pub(crate) struct DiscordTextDelivery {
    http: Arc<Http>,
}

#[async_trait]
impl TextDelivery for DiscordTextDelivery {
    /// Discord のチャンネル ID は数値スノーフレーク。移設前の
    /// `channel_id_str.parse::<u64>()` と同じ検査・同じ文言（fail-closed）。
    fn validate_target(&self, target: &str) -> Result<(), String> {
        match target.parse::<u64>() {
            Ok(_) => Ok(()),
            Err(_) => Err(format!("無効なchannel_id: {target}")),
        }
    }

    fn mention(&self, user_id: &str) -> String {
        format!("<@{user_id}>")
    }

    fn chunk_limit(&self) -> usize {
        DISCORD_CHUNK_LIMIT
    }

    async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
        // 検査済みのはずだが、境界では必ず自分で確かめる（"" で送らない）。
        let channel_id: u64 = target
            .parse()
            .map_err(|_| format!("無効なchannel_id: {target}"))?;
        ChannelId::new(channel_id)
            .send_message(&self.http, CreateMessage::new().content(text))
            .await
            .map(|_| ())
            // 文言は移設前と同じ serenity の Display をそのまま使う（呼び出し側が
            // 「N/M 通送信済み」の文へ埋める）。
            .map_err(|e| e.to_string())
    }
}

impl DiscordGatewayActions {
    /// この gateway が提供する素テキスト配送口を組む。
    pub(super) fn build_text_delivery(&self) -> DiscordTextDelivery {
        DiscordTextDelivery {
            http: self.http.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery() -> DiscordTextDelivery {
        DiscordTextDelivery {
            http: Arc::new(Http::new("dummy-token")),
        }
    }

    /// 宛先の検査と文言（移設前の `無効なchannel_id: …` をバイト単位で維持）。
    #[test]
    fn validates_numeric_channel_ids_only() {
        let d = delivery();
        assert!(d.validate_target("123456789").is_ok());
        // 2^53 を超えるスノーフレークも通る。
        assert!(d.validate_target("1234567890123456789").is_ok());
        assert_eq!(
            d.validate_target("not-a-number").unwrap_err(),
            "無効なchannel_id: not-a-number"
        );
        assert_eq!(d.validate_target("").unwrap_err(), "無効なchannel_id: ");
        assert_eq!(d.validate_target("-1").unwrap_err(), "無効なchannel_id: -1");
    }

    /// メンションの記法は transport 側の知識（汎用層は `<@…>` を組まない）。
    #[test]
    fn mention_uses_discord_notation() {
        assert_eq!(delivery().mention("42"), "<@42>");
    }

    /// 1 通の上限は webhook 配送と同じ安全長を共有する（Discord の 2000 未満）。
    #[test]
    fn chunk_limit_matches_the_discord_safe_length() {
        assert_eq!(delivery().chunk_limit(), DISCORD_CHUNK_LIMIT);
        assert!(delivery().chunk_limit() < 2000);
    }
}
