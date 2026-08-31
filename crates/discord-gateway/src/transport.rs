//! Discord 外部 I/O（REST）を隔離する層。serenity（token 保持）とハーネス dry-run をここに閉じ、
//! 上位（ops / post）は三結果だけを見る。core には一切出さない（設計 §1.2/§1.3）。

use async_trait::async_trait;
use serde_json::{json, Value};

/// transport の三結果（§5.3 に写す前段）。
pub enum TransportOutcome {
    /// 外部 API が受理したと確認した。生 JSON（resolve）や要約（write）を運ぶ。
    Ok(Value),
    /// 外部 I/O 0 または確定非受理（不正入力・4xx client error）。
    Rejected,
    /// 受理成否が不明（timeout / 接続断 / 5xx / 429）。捏造しない（§5.3）。
    Indeterminate,
}

/// dry-run が say/reply/reaction/resolve を残す tracing target。テスト/QC がこの target で拾う。
pub const DRY_RUN_LOG_TARGET: &str = "opencrab_discordgate::dry_run";

/// Discord REST 抽象。real は serenity、QC は dry-run。
#[async_trait]
pub trait DiscordTransport: Send + Sync {
    /// channel への通常投稿（say）。
    async fn create_message(&self, channel_id: &str, content: &str) -> TransportOutcome;
    /// message への返信（reply DI）。
    async fn reply_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> TransportOutcome;
    /// message への reaction（reaction DI）。
    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome;
    /// gateway 自動付与の system reaction（👀/🏁/❌）。production の REST は `add_reaction` と同一
    /// （create_reaction は同一絵文字を冪等に扱う）。dry-run では観測用に kind を分ける（agent の
    /// reaction DI と混同しないため）。
    async fn add_system_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome;
    /// message の生 JSON 取得（resolve eN・読み取り）。
    async fn get_message(&self, channel_id: &str, message_id: &str) -> TransportOutcome;
    /// user の生 JSON 取得（resolve uN・読み取り）。
    async fn get_user(&self, user_id: &str) -> TransportOutcome;
}

/// QC 用 dry-run transport。REST を叩かず種別・対象・本文を INFO ログに残し Ok を返す。
/// これにより reply/reaction/resolve/say の**実 DI 経路**（invoke→決着→resume）をトークン・
/// ネットワーク無しで検証できる（Nostr の say dry-run を全能力へ広げた形）。
pub struct DryRunTransport;

impl DryRunTransport {
    fn log(kind: &str, channel: &str, message: &str, emoji: &str, body: &str) -> TransportOutcome {
        tracing::info!(
            target: DRY_RUN_LOG_TARGET,
            kind,
            channel,
            message,
            emoji,
            body = %body,
            "DRY_RUN discord op (not sent)"
        );
        TransportOutcome::Ok(json!({"dry_run": true, "kind": kind}))
    }
}

#[async_trait]
impl DiscordTransport for DryRunTransport {
    async fn create_message(&self, channel_id: &str, content: &str) -> TransportOutcome {
        Self::log("say", channel_id, "", "", content)
    }
    async fn reply_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> TransportOutcome {
        Self::log("reply", channel_id, message_id, "", content)
    }
    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome {
        Self::log("reaction", channel_id, message_id, emoji, "")
    }
    async fn add_system_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome {
        Self::log("system_reaction", channel_id, message_id, emoji, "")
    }
    async fn get_message(&self, channel_id: &str, message_id: &str) -> TransportOutcome {
        // resolve は生 JSON を返す約束。dry-run は取得できないので短縮参照だけ返す。
        TransportOutcome::Ok(json!({
            "dry_run": true, "kind": "resolve",
            "channel_id": channel_id, "message_id": message_id
        }))
    }
    async fn get_user(&self, user_id: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({"dry_run": true, "kind": "resolve", "user_id": user_id}))
    }
}

// ==================== real serenity transport ====================

use serenity::all::{ChannelId, CreateMessage, MessageId, ReactionType, UserId};
use serenity::http::Http;
use std::sync::Arc;

/// production transport。bot token を保持する serenity Http だけを持つ（token は他へ出さない）。
pub struct SerenityTransport {
    http: Arc<Http>,
}

impl SerenityTransport {
    /// token は env から受けた値のみ（[`crate::secret::take_bot_token`]）。ここから外へ出さない。
    pub fn new(token: &str) -> Self {
        Self {
            http: Arc::new(Http::new(token)),
        }
    }

    pub fn http(&self) -> Arc<Http> {
        self.http.clone()
    }
}

/// snowflake 文字列 → serenity Id。非 snowflake は None（確定 Rejected へ）。
fn channel(id: &str) -> Option<ChannelId> {
    id.parse::<u64>().ok().map(ChannelId::new)
}
fn message(id: &str) -> Option<MessageId> {
    id.parse::<u64>().ok().map(MessageId::new)
}

/// write の serenity Err を三結果へ。4xx（≠429）= 確定非受理、5xx/429/network = 不明（§5.3）。
fn classify_write_err(e: &serenity::Error) -> TransportOutcome {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(resp)) = e {
        let code = resp.status_code.as_u16();
        if (400..500).contains(&code) && code != 429 {
            tracing::warn!(status = code, "discord write rejected by API");
            return TransportOutcome::Rejected;
        }
    }
    tracing::warn!(error = %crate::secret::redact_token(&e.to_string()), "discord write outcome unknown");
    TransportOutcome::Indeterminate
}

#[async_trait]
impl DiscordTransport for SerenityTransport {
    async fn create_message(&self, channel_id: &str, content: &str) -> TransportOutcome {
        let Some(ch) = channel(channel_id) else {
            return TransportOutcome::Rejected;
        };
        match ch.say(&self.http, content).await {
            Ok(m) => TransportOutcome::Ok(json!({"message_id": m.id.get().to_string()})),
            Err(e) => classify_write_err(&e),
        }
    }

    async fn reply_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> TransportOutcome {
        let (Some(ch), Some(mid)) = (channel(channel_id), message(message_id)) else {
            return TransportOutcome::Rejected;
        };
        let builder = CreateMessage::new()
            .content(content)
            .reference_message((ch, mid));
        match ch.send_message(&self.http, builder).await {
            Ok(m) => TransportOutcome::Ok(json!({"message_id": m.id.get().to_string()})),
            Err(e) => classify_write_err(&e),
        }
    }

    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome {
        let (Some(ch), Some(mid)) = (channel(channel_id), message(message_id)) else {
            return TransportOutcome::Rejected;
        };
        match ch
            .create_reaction(&self.http, mid, ReactionType::Unicode(emoji.to_string()))
            .await
        {
            Ok(()) => TransportOutcome::Ok(json!({"reacted": true})),
            Err(e) => classify_write_err(&e),
        }
    }

    async fn add_system_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome {
        // production では agent reaction と同一 REST（create_reaction）。dry-run だけ kind を分ける。
        self.add_reaction(channel_id, message_id, emoji).await
    }

    async fn get_message(&self, channel_id: &str, message_id: &str) -> TransportOutcome {
        let (Some(ch), Some(mid)) = (channel(channel_id), message(message_id)) else {
            return TransportOutcome::Rejected;
        };
        match ch.message(&self.http, mid).await {
            Ok(m) => match serde_json::to_value(&m) {
                Ok(v) => TransportOutcome::Ok(v),
                Err(_) => TransportOutcome::Rejected,
            },
            // 読み取りは副作用なし。失敗は確定 Rejected（LLM が必要なら再試行）。
            Err(_) => TransportOutcome::Rejected,
        }
    }

    async fn get_user(&self, user_id: &str) -> TransportOutcome {
        let Some(uid) = user_id.parse::<u64>().ok().map(UserId::new) else {
            return TransportOutcome::Rejected;
        };
        match self.http.get_user(uid).await {
            Ok(u) => match serde_json::to_value(&u) {
                Ok(v) => TransportOutcome::Ok(v),
                Err(_) => TransportOutcome::Rejected,
            },
            Err(_) => TransportOutcome::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dry_run_reply_and_reaction_are_ok_without_network() {
        let t = DryRunTransport;
        assert!(matches!(
            t.reply_message("100", "200", "hi").await,
            TransportOutcome::Ok(_)
        ));
        assert!(matches!(
            t.add_reaction("100", "200", "👍").await,
            TransportOutcome::Ok(_)
        ));
        assert!(matches!(
            t.create_message("100", "hello").await,
            TransportOutcome::Ok(_)
        ));
        assert!(matches!(
            t.add_system_reaction("100", "200", "👀").await,
            TransportOutcome::Ok(_)
        ));
    }
}
