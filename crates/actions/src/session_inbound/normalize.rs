use crate::transcript::InboundMessageRecord;

/// ゲートが core に渡す正規化済み受信 1 件。
///
/// 機械的な配送ハンドル（HTTP・描画）は含めない。`session_id` の書式はゲート側の
/// 現行規約のまま（Discord なら `discord-{agent}-{guild}-{channel}`）。
#[derive(Debug, Clone)]
pub struct NormalizedInbound<'a> {
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub sender_id: &'a str,
    pub sender_name: &'a str,
    pub avatar_url: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    pub pubkey: Option<&'a str>,
    pub text: &'a str,
    pub image_urls: &'a [String],
    pub external_id: &'a str,
}

impl<'a> NormalizedInbound<'a> {
    pub(crate) fn as_record(&self) -> InboundMessageRecord<'a> {
        InboundMessageRecord {
            session_id: self.session_id,
            recipient_agent_id: self.agent_id,
            sender_id: self.sender_id,
            sender_name: self.sender_name,
            avatar_url: self.avatar_url,
            channel_id: self.channel_id,
            pubkey: self.pubkey,
            text: self.text,
            image_urls: self.image_urls,
        }
    }
}

/// ゲートが inbound 1 口へ渡す正規化イベント（生識別子のみ。権限の真偽は載せない）。
#[derive(Debug, Clone, Copy)]
pub struct NormalizedInboundEvent<'a> {
    pub sender_id: &'a str,
    pub channel_id: &'a str,
    /// 空なら DM。
    pub guild_id: &'a str,
}
