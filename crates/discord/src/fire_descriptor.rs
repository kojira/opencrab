//! Discord の時刻発火 descriptor（#628）。
//!
//! transport が「自分の発火先としての性質と ID 書式」を名乗る [`opencrab_actions::TransportFire`]
//! の Discord 実装。旧 `opencrab_db::queries::SessionFireTarget::DiscordChannel` の挙動を厳密に
//! 写す（parse / build / G ゲート対象 / 応答本文の自動配送）。**db 層から transport の知識を
//! 撤去する**ための移設先で、Discord を足す / 変える作業がこの crate 内で完結するようにする。

use opencrab_actions::{gateway_kinds, FireTarget, TransportFire, TransportFireEnv};

/// `discord-{agent}-{guild}-{channel}` の発火先を名乗る descriptor。
///
/// **性質**（旧 enum から不変）: live G マスタゲートの対象（`is_g_gated=true`）／発火ターンの
/// 応答本文はそのままチャンネルへ自動配送される（`posts_response_body=true`）。
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

    fn posts_response_body(&self) -> bool {
        true
    }

    fn human_hint(&self) -> &'static str {
        "Discord のチャンネル"
    }

    /// この Discord ゲートウェイが設定上「立ち上がるべき」か（実行時述語・条件 D）。
    ///
    /// 判定は旧 `main.rs` の `discord_expected` を厳密に写す: TOML の共有ゲートウェイが
    /// 設定されている（`configured_shared_kinds` に自分の kind がある）か、per-agent の
    /// 有効な Discord 設定が db にある（#602 の本番対象はまさに TOML に無い per-agent）。
    fn should_be_running(&self, env: &TransportFireEnv) -> bool {
        if env
            .configured_shared_kinds
            .contains(&gateway_kinds::DISCORD)
        {
            return true;
        }
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
