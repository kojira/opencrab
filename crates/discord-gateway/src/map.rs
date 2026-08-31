//! Discord Message Create ⇄ V3 said の写像と origin/address 規約。
//!
//! 会話の e/u/c 採番と §9A レンダリングは core（conversation.rs の汎用機構）が行う。gateway は
//! said に生の origin（stable anchor）と author（Discord 認証済み sender・#848）と本文を載せるだけで、
//! core に Discord 語彙を足さない。origin は Nostr の `nostr:event:v1:...` に倣った版付き anchor。

use serde::Deserialize;

/// 受信 Discord message（Serenity Message Create 相当・fixture でも同形）。生 ID は会話へ出さない。
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    /// message snowflake（10進文字列）。
    pub id: String,
    /// channel snowflake（10進文字列）。
    pub channel_id: String,
    /// guild snowflake（DM は None）。
    #[serde(default)]
    pub guild_id: Option<String>,
    pub author: IncomingAuthor,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IncomingAuthor {
    /// user snowflake（10進文字列）。**Discord 認証済み sender**。said の AuthorId に刻む（#848）。
    pub id: String,
    /// bot フラグ（自分以外の bot は通す・設計 §5.1）。
    #[serde(default)]
    pub bot: bool,
    #[serde(default)]
    pub username: Option<String>,
}

/// said に載せる写像結果。origin/author は core が e/u 番号へ写す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedSaid {
    pub origin: String,
    pub author_id: String,
    pub text: String,
}

/// message の stable anchor origin。`discord:message:v1:{channel}:{message}`。
/// core はこの origin 文字列を初出順に e番号へ写す（platform 非依存）。
pub fn origin_for(channel_id: &str, message_id: &str) -> String {
    format!("discord:message:v1:{channel_id}:{message_id}")
}

/// origin から (channel_id, message_id) を取り出す。gateway が REST（reply/reaction/resolve）で使う。
pub fn parse_origin(origin: &str) -> Option<(String, String)> {
    let rest = origin.strip_prefix("discord:message:v1:")?;
    let (channel, message) = rest.split_once(':')?;
    if is_decimal(channel) && is_decimal(message) {
        Some((channel.to_string(), message.to_string()))
    } else {
        None
    }
}

/// binding address（= session id）を組む: `discord-{agent_id}-{guild_id}-{channel_id}`。
/// DM は guild 成分が空。agent_id は core `agents.id`。
pub fn address_for(agent_id: &str, guild_id: &str, channel_id: &str) -> String {
    format!("discord-{agent_id}-{guild_id}-{channel_id}")
}

/// 既知の agent_id を前提に address から (guild_id, channel_id) を取り出す。
/// agent_id 自体が `-` を含みうる（UUID）ため、既知 prefix を剥がしてから末尾 2 成分を分ける。
/// guild/channel は snowflake（`-` を含まない）なので後方から確実に切れる。DM は guild="".
pub fn parse_address(agent_id: &str, address: &str) -> Option<(String, String)> {
    let prefix = format!("discord-{agent_id}-");
    let rest = address.strip_prefix(&prefix)?;
    let (guild, channel) = rest.rsplit_once('-')?;
    // channel は非空 snowflake。guild は空（DM）か snowflake。
    if channel.is_empty() || !is_decimal(channel) {
        return None;
    }
    if !guild.is_empty() && !is_decimal(guild) {
        return None;
    }
    Some((guild.to_string(), channel.to_string()))
}

/// 受信 message を said へ写す。**自分自身の投稿だけ**除外する（他 bot・owner・trusted は core が
/// admission する・設計 §5.1）。author/channel/message id が非 snowflake なら None（不正入力を落とす）。
pub fn map_message(msg: &IncomingMessage, self_bot_id: &str) -> Option<MappedSaid> {
    if msg.author.id == self_bot_id {
        return None;
    }
    if !is_decimal(&msg.author.id) || !is_decimal(&msg.channel_id) || !is_decimal(&msg.id) {
        return None;
    }
    Some(MappedSaid {
        origin: origin_for(&msg.channel_id, &msg.id),
        author_id: msg.author.id.clone(),
        text: msg.content.clone(),
    })
}

/// 受信 message の binding address（core が bind ack していれば購読集合）。
pub fn address_of(agent_id: &str, msg: &IncomingMessage) -> String {
    let guild = msg.guild_id.as_deref().unwrap_or("");
    address_for(agent_id, guild, &msg.channel_id)
}

/// fixture / serenity 由来の 1 行を IncomingMessage へ。パース不能は None（行を捨てる）。
pub fn parse_event_line(line: &str) -> Option<IncomingMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, channel: &str, guild: Option<&str>, author: &str, content: &str) -> IncomingMessage {
        IncomingMessage {
            id: id.into(),
            channel_id: channel.into(),
            guild_id: guild.map(str::to_string),
            author: IncomingAuthor {
                id: author.into(),
                bot: false,
                username: Some("someone".into()),
            },
            content: content.into(),
        }
    }

    #[test]
    fn origin_roundtrip() {
        let o = origin_for("100", "200");
        assert_eq!(o, "discord:message:v1:100:200");
        assert_eq!(parse_origin(&o), Some(("100".into(), "200".into())));
        assert_eq!(parse_origin("nostr:event:v1:default:aa"), None);
        assert_eq!(parse_origin("discord:message:v1:xx:200"), None);
    }

    #[test]
    fn address_roundtrip_guild_and_dm() {
        let a = address_for("agent-uuid-with-dash", "500", "600");
        assert_eq!(a, "discord-agent-uuid-with-dash-500-600");
        assert_eq!(
            parse_address("agent-uuid-with-dash", &a),
            Some(("500".into(), "600".into()))
        );
        // DM: guild 空。
        let dm = address_for("agent-uuid-with-dash", "", "600");
        assert_eq!(dm, "discord-agent-uuid-with-dash--600");
        assert_eq!(
            parse_address("agent-uuid-with-dash", &dm),
            Some(("".into(), "600".into()))
        );
        // 別 agent の address は自分の prefix で剥がれない。
        assert_eq!(parse_address("other-agent", &a), None);
    }

    #[test]
    fn map_excludes_only_self() {
        let self_bot = "111";
        // 自分の投稿は除外。
        assert_eq!(map_message(&msg("1", "100", Some("500"), "111", "hi"), self_bot), None);
        // 他人（他 bot 含む）は通す。author=Discord 認証済み sender を刻む。
        let mut m = msg("2", "100", Some("500"), "222", "hello");
        m.author.bot = true;
        let mapped = map_message(&m, self_bot).unwrap();
        assert_eq!(mapped.origin, "discord:message:v1:100:2");
        assert_eq!(mapped.author_id, "222", "author は Discord 認証済み sender（#848）");
        assert_eq!(mapped.text, "hello");
    }

    #[test]
    fn map_rejects_non_snowflake_ids() {
        assert_eq!(map_message(&msg("x", "100", None, "222", "hi"), "111"), None);
        assert_eq!(map_message(&msg("1", "yy", None, "222", "hi"), "111"), None);
        assert_eq!(map_message(&msg("1", "100", None, "zz", "hi"), "111"), None);
    }

    #[test]
    fn address_of_uses_empty_guild_for_dm() {
        let m = msg("1", "600", None, "222", "hi");
        assert_eq!(address_of("agent-x", &m), "discord-agent-x--600");
        let g = msg("1", "600", Some("500"), "222", "hi");
        assert_eq!(address_of("agent-x", &g), "discord-agent-x-500-600");
    }

    #[test]
    fn parse_event_line_reads_message_json() {
        let line = r#"{"id":"7","channel_id":"100","guild_id":"500","author":{"id":"222","bot":false,"username":"al"},"content":"やあ"}"#;
        let m = parse_event_line(line).unwrap();
        assert_eq!(m.id, "7");
        assert_eq!(m.channel_id, "100");
        assert_eq!(m.guild_id.as_deref(), Some("500"));
        assert_eq!(m.author.id, "222");
        assert_eq!(m.content, "やあ");
        assert!(parse_event_line("  ").is_none());
        assert!(parse_event_line("not json").is_none());
    }
}
