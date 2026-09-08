//! Discord の時刻発火 descriptor（#628）。
//!
//! transport が「自分の発火先としての性質と ID 書式」を名乗る [`opencrab_actions::TransportFire`]
//! の Discord 実装。parse / build / Gゲート対象を定義し、db層からtransport知識を
//! 撤去する**ための移設先で、Discord を足す / 変える作業がこの crate 内で完結するようにする。

use opencrab_actions::{gateway_kinds, FireTarget, TransportFire, TransportFireEnv};

/// `discord-{agent}-{guild}-{channel}` の発火先を名乗る descriptor。
///
/// **性質**（旧 enum から不変）: live G マスタゲートの対象（`is_g_gated=true`）。発火ターンの
/// 応答本文はそのままチャンネルへ自動配送される（誘導文言は transport 非依存・#925 §1.7 で
/// `posts_response_body` は撤去）。
pub struct DiscordFire;

impl TransportFire for DiscordFire {
    fn kind(&self) -> &'static str {
        gateway_kinds::DISCORD
    }

    /// `session_id` を保存済み `agent_id` で剥がして発火先を導く（旧 `resolve_session_fire_target`
    /// の Discord 分岐を厳密に写す）。
    ///
    /// **naive な `split('-')` は禁止**（`agent_id` は UUID でハイフンを含む）。保存済み `agent_id`
    /// で接頭辞を剥がし、残りの guild/channel が数値（ハイフン無し）であることを確認する。合致
    /// しなければ `None`（fail-closed）。
    fn parse(&self, session_id: &str, agent_id: &str) -> Option<FireTarget> {
        let prefix = format!("discord-{agent_id}-");
        let rest = session_id.strip_prefix(&prefix)?;
        // rest = "{guild}-{channel}"。guild/channel は数値（ハイフン無し）なので rsplit_once 安全。
        let (guild, channel) = rest.rsplit_once('-')?;
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if numeric(guild) && numeric(channel) {
            Some(FireTarget {
                kind: gateway_kinds::DISCORD,
                channel_id: channel.to_string(),
                guild_id: guild.to_string(),
                route: String::new(),
            })
        } else {
            None
        }
    }

    /// [`parse`](Self::parse) の逆写像。`discord-{agent}-{guild}-{channel}` を組む。
    fn build_session_id(&self, target: &FireTarget, agent_id: &str) -> String {
        format!(
            "discord-{agent_id}-{}-{}",
            target.guild_id, target.channel_id
        )
    }

    fn is_g_gated(&self) -> bool {
        true
    }

    fn human_hint(&self) -> &'static str {
        "Discord のチャンネル"
    }

    /// Enabled per-agent V3 configurations determine whether Discord should be running.
    fn should_be_running(&self, env: &TransportFireEnv) -> bool {
        opencrab_db::queries::list_enabled_agent_discord_configs(env.conn)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    fn sample_target(&self) -> FireTarget {
        FireTarget {
            kind: gateway_kinds::DISCORD,
            channel_id: "2002".to_string(),
            guild_id: "1001".to_string(),
            route: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_UUID: &str = "11111111-1111-4111-8111-111111111111";

    /// UUID（ハイフン入り）を保存済み agent_id で剥がすので割れない。
    #[test]
    fn parse_strips_uuid_prefix() {
        let sid = format!("discord-{AGENT_UUID}-1001-2002");
        assert_eq!(
            DiscordFire.parse(&sid, AGENT_UUID),
            Some(FireTarget {
                kind: gateway_kinds::DISCORD,
                channel_id: "2002".to_string(),
                guild_id: "1001".to_string(),
                route: String::new(),
            })
        );
    }

    /// 非数値 guild/channel・別 agent_id・別種別は None（fail-closed）。
    #[test]
    fn parse_fail_closed() {
        assert!(DiscordFire
            .parse(&format!("discord-{AGENT_UUID}-guild-chan"), AGENT_UUID)
            .is_none());
        assert!(DiscordFire
            .parse(&format!("discord-{AGENT_UUID}-1001-2002"), "other-agent")
            .is_none());
        assert!(DiscordFire
            .parse(&format!("nostr-{AGENT_UUID}"), AGENT_UUID)
            .is_none());
    }

    /// build ↔ parse の round-trip（両方向）。
    #[test]
    fn build_is_inverse_of_parse() {
        let sample = DiscordFire.sample_target();
        let sid = DiscordFire.build_session_id(&sample, AGENT_UUID);
        assert_eq!(sid, format!("discord-{AGENT_UUID}-1001-2002"));
        assert_eq!(DiscordFire.parse(&sid, AGENT_UUID), Some(sample));
    }
}
