//! say（通常発言）配送。core からの say を binding の channel への**通常投稿**として送る。
//! 返信は明示 `reply` DI 能力が担うので、say に reply target を暗黙設定しない（設計 §6.1・DI-16）。
//! dry-run / production の分岐は [`crate::transport`] の実装差で吸収する（say も invoke も同一 transport）。

use std::sync::Arc;

use crate::transport::{DiscordTransport, TransportOutcome};

/// say の配送結果（観測性用）。
#[derive(Debug, PartialEq, Eq)]
pub enum SayDelivery {
    /// channel へ通常投稿した（dry-run 含む）。`message_id` は投稿できた**自分のメッセージ**の
    /// snowflake（transport の create_message 応答から得る）。🏁（完了サイン）はこの id へ付ける
    /// ——発端ではなく自分の発言に付けるのが正（owner 裁定 row 345）。取得できなければ None。
    Posted { message_id: Option<String> },
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
        TransportOutcome::Ok(v) => {
            // create_message は Ok に投稿できたメッセージ id を載せる（production=serenity 実 id・
            // dry-run=合成 id）。🏁 の付け先（自分の投稿）に使う。
            let message_id = v
                .get("message_id")
                .and_then(|m| m.as_str())
                .map(str::to_string);
            SayDelivery::Posted { message_id }
        }
        TransportOutcome::Rejected => SayDelivery::Failed("rejected".into()),
        TransportOutcome::Indeterminate => SayDelivery::Failed("indeterminate".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DryRunTransport;

    #[tokio::test]
    async fn dry_run_say_is_posted_with_own_message_id() {
        let t: Arc<dyn DiscordTransport> = Arc::new(DryRunTransport);
        // dry-run でも自分の投稿 id を持つ（🏁 の付け先を QC が観測できるようにする）。
        match deliver_say(&t, "100", "hello").await {
            SayDelivery::Posted { message_id } => {
                assert!(
                    message_id.is_some(),
                    "dry-run say は自分の message id を返す"
                );
            }
            other => panic!("expected Posted, got {other:?}"),
        }
    }
}
