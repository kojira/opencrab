use crate::transcript::InboundMessageRecord;

/// ゲートが core に渡す正規化済み受信 1 件。
///
/// 機械的な配送ハンドル（HTTP・描画）は含めない。`session_id` は core のセッション。
#[derive(Debug, Clone)]
pub struct NormalizedInbound<'a> {
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub sender_id: &'a str,
    pub sender_name: &'a str,
    pub avatar_url: Option<&'a str>,
    /// The gateway's opaque key of this session (metadata `session_key`, D-1099).
    pub session_key: Option<&'a str>,
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
            session_key: self.session_key,
            pubkey: self.pubkey,
            text: self.text,
            image_urls: self.image_urls,
        }
    }
}
