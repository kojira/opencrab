//! ゲートウェイ非依存な転記の語彙（#158 S3 / #156 S6）。
//!
//! 以前は「このターンが何で起動されたか」を表す型（`DiscordReplyContext`）と A2UI 応答の
//! 記録内容（`InteractionRecord`）が `crates/discord` に置かれていた。中身は文字列と数値
//! だけで transport 依存の型を一切含まないのに、`crates/server` の転記モジュールが
//! discord crate の型を引くため、記録の関数が `#[cfg(feature = "discord")]` の配下に
//! 落ちていた（discord を切ると Nostr/REST と同じ形の記録まで消える）。ここへ移すことで
//! 記録の形が transport の機能フラグから独立する。
//!
//! **記録される JSON は移設前とバイト等価**であることが不変条件。`session_logs.
//! metadata_json` の `source` 値は web の表示（セッション詳細 / セッションカード）が
//! 文字列比較で読んでいるため、値を変えると web を同時に直す必要がある。値を変えないので
//! web は無変更で動く。

/// 転記元が決めたopaqueなsource pair。
///
/// 共有層は値を列挙・解釈しない。具体gatewayが自分のcrate内で定数を構築し、受信と返信の
/// 対を型として渡すことで、共有層での綴り違いだけを防ぐ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSource {
    inbound: &'static str,
    reply: &'static str,
}

impl TranscriptSource {
    pub const fn new(inbound: &'static str, reply: &'static str) -> Self {
        Self { inbound, reply }
    }

    pub const fn inbound(self) -> &'static str {
        self.inbound
    }

    pub const fn reply(self) -> &'static str {
        self.reply
    }
}

/// ゲートウェイから受信した発言の記録内容。
///
/// `channel_id` / `pubkey` は **metadata のキー名そのもの**で、由来ごとに異なる識別子を
/// 落とさないために両方を持つ（Discord は宛先チャンネル、Nostr は送信者公開鍵）。
/// `None` のキーは metadata に出さない ＝ 移設前の各サイトの形がそのまま再現される。
#[derive(Debug, Clone)]
pub struct InboundMessageRecord<'a> {
    pub session_id: &'a str,
    /// **受信側**エージェントの id（`session_logs.agent_id` 列に入る）。
    ///
    /// 記憶索引・FTS 記憶検索は `WHERE agent_id = <当該エージェント>` で走査するため、
    /// 受信（相手の発言）も**受信側エージェントの名義**で記録しないと、そのエージェントの
    /// 索引・検索に一切載らない（#377）。相手の識別は下の `sender_id`（`speaker_id` 列）が担う。
    pub recipient_agent_id: &'a str,
    /// 発言者（送信者）の id。`session_logs.speaker_id` 列に入る
    /// （Discord のユーザー ID / Nostr の送信者 pubkey）。
    pub sender_id: &'a str,
    /// 表示名（metadata `user_name`）。
    pub sender_name: &'a str,
    /// アイコン URL（metadata `user_avatar_url`）。
    pub avatar_url: Option<&'a str>,
    /// 宛先チャンネル（metadata `channel_id`）。web の表示が参照するので落とさない。
    pub channel_id: Option<&'a str>,
    /// 送信者公開鍵（metadata `pubkey`）。
    pub pubkey: Option<&'a str>,
    /// `session_logs.content` に入る本文。
    pub text: &'a str,
    /// 添付画像 URL（空なら metadata に出さない）。
    pub image_urls: &'a [String],
}

/// エージェントがゲートウェイへ返した応答の記録内容。
#[derive(Debug, Clone)]
pub struct OutboundReplyRecord<'a> {
    pub agent_id: &'a str,
    pub session_id: &'a str,
    /// 宛先チャンネル（metadata `channel_id`）。web の表示が参照するので落とさない。
    pub channel_id: Option<&'a str>,
    /// `session_logs.content` に入る本文。
    pub text: &'a str,
    /// このターンの起動要因。記録しない由来（Nostr）は `None`。
    pub context: Option<AgentReplyContext<'a>>,
}

/// このターンが何で起動されたかを表す（旧 `opencrab_discord::DiscordReplyContext`）。
///
/// metadata の `triggered_by` 等の差分を型で表現する。記録の実体（`SessionLogRow` の
/// 組み立てと書き込みポリシー）は server 側の transcript モジュールが所有する。
#[derive(Debug, Clone)]
pub enum AgentReplyContext<'a> {
    /// 新着メッセージへの直接応答（metadata `tool_calls_made`）。
    Direct { tool_calls_made: usize },
    /// サブタスク完了を受けた再呼び出しの応答（`triggered_by = "subtask_completed"`）。
    SubtaskCompleted,
    /// A2UI インタラクション応答を受けた再呼び出しの応答
    /// （`triggered_by = "interaction_response"` + `interaction_id`）。
    InteractionResponse { interaction_id: &'a str },
}

/// A2UI インタラクションの記録内容（旧 `opencrab_discord::InteractionRecord`、#42）。
#[derive(Debug, Clone)]
pub struct InteractionRecord<'a> {
    pub interaction_id: &'a str,
    pub surface_id: &'a str,
    pub action_name: &'a str,
    pub component_id: &'a str,
    pub responder_id: &'a str,
    /// `session_logs.content` に書く整形済みテキスト。
    pub content: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_pair_is_preserved() {
        let source = TranscriptSource::new("inbound", "reply");
        assert_eq!(source.inbound(), "inbound");
        assert_eq!(source.reply(), "reply");
    }
}
