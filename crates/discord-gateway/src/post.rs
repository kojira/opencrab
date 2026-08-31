//! say（通常発言）配送。core からの say を binding の channel への**通常投稿**として送る。
//! 返信は明示 `reply` DI 能力が担うので、say に reply target を暗黙設定しない（設計 §6.1・DI-16）。
//! dry-run / production の分岐は [`crate::transport`] の実装差で吸収する（say も invoke も同一 transport）。

use std::sync::Arc;

use crate::transport::{DiscordTransport, TransportOutcome};

/// say の配送結果（観測性用）。
#[derive(Debug, PartialEq, Eq)]
pub enum SayDelivery {
    /// channel へ通常投稿した（dry-run 含む）。
    Posted,
    /// 投稿失敗（確定拒否・不明どちらも会話配送の失敗として観測）。
    Failed(String),
}

/// say を channel の通常投稿として配送する。`channel_id` は binding address から解決する。
pub async fn deliver_say(
    transport: &Arc<dyn DiscordTransport>,
    channel_id: &str,
    text: &str,
) -> SayDelivery {
    match transport.create_message(channel_id, text).await {
        TransportOutcome::Ok(_) => SayDelivery::Posted,
        TransportOutcome::Rejected => SayDelivery::Failed("rejected".into()),
        TransportOutcome::Indeterminate => SayDelivery::Failed("indeterminate".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DryRunTransport;

    #[tokio::test]
    async fn dry_run_say_is_posted() {
        let t: Arc<dyn DiscordTransport> = Arc::new(DryRunTransport);
        assert_eq!(deliver_say(&t, "100", "hello").await, SayDelivery::Posted);
    }
}
