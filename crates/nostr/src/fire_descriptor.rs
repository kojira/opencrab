//! Nostr の時刻発火 descriptor（#628）。
//!
//! transport が「自分の発火先としての性質と ID 書式」を名乗る [`opencrab_actions::TransportFire`]
//! の Nostr 実装。旧 `opencrab_db::queries::SessionFireTarget::NostrBroadcast` の挙動を厳密に
//! 写す。Nostr broadcast は特定チャンネルを持たない（channel/guild は空）。

use opencrab_actions::{gateway_kinds, FireTarget, TransportFire, TransportFireEnv};

use crate::session::NOSTR_SESSION_PREFIX;

/// `nostr-{agent}` の発火先を名乗る descriptor。
///
/// **性質**（旧 enum から不変）: live G マスタゲートの対象外（`is_g_gated=false`）。
/// （旧 `posts_response_body` は #925 §1.7 で撤去。V3 の Nostr 発火は extgate レーンが担い、
/// 応答本文＝gateway への say としてタイムラインへ投稿される。）
pub struct NostrFire;

impl TransportFire for NostrFire {
    fn kind(&self) -> &'static str {
        gateway_kinds::NOSTR
    }

    /// `nostr-{agent}` に完全一致すれば自分の発火先（旧 `resolve_session_fire_target` の
    /// Nostr 分岐を厳密に写す）。channel/guild は持たない（broadcast）。
    fn parse(&self, session_id: &str, agent_id: &str) -> Option<FireTarget> {
        if session_id == format!("{NOSTR_SESSION_PREFIX}{agent_id}") {
            Some(FireTarget {
                kind: gateway_kinds::NOSTR,
                channel_id: String::new(),
                guild_id: String::new(),
                route: String::new(),
            })
        } else {
            None
        }
    }

    /// [`parse`](Self::parse) の逆写像。`nostr-{agent}` を組む。
    fn build_session_id(&self, _target: &FireTarget, agent_id: &str) -> String {
        format!("{NOSTR_SESSION_PREFIX}{agent_id}")
    }

    fn is_g_gated(&self) -> bool {
        false
    }

    fn human_hint(&self) -> &'static str {
        "Nostr の自発投稿"
    }

    /// この Nostr sub-gateway が設定上「立ち上がるべき」か（実行時述語・条件 D）。
    ///
    /// 判定は旧 `main.rs` の `nostr_expected` を厳密に写す: 有効な per-agent Nostr 設定が
    /// db にあるか。
    fn should_be_running(&self, env: &TransportFireEnv) -> bool {
        opencrab_db::queries::list_enabled_agent_nostr_configs(env.conn)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    fn sample_target(&self) -> FireTarget {
        FireTarget {
            kind: gateway_kinds::NOSTR,
            channel_id: String::new(),
            guild_id: String::new(),
            route: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_UUID: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn parse_matches_exact_and_carries_no_channel() {
        assert_eq!(
            NostrFire.parse(&format!("nostr-{AGENT_UUID}"), AGENT_UUID),
            Some(FireTarget {
                kind: gateway_kinds::NOSTR,
                channel_id: String::new(),
                guild_id: String::new(),
                route: String::new(),
            })
        );
    }

    #[test]
    fn parse_fail_closed() {
        // 別 agent_id・別種別は None。
        assert!(NostrFire
            .parse(&format!("nostr-{AGENT_UUID}"), "other")
            .is_none());
        assert!(NostrFire
            .parse(&format!("discord-{AGENT_UUID}-1-2"), AGENT_UUID)
            .is_none());
    }

    /// build ↔ parse の round-trip（#508 の要点: Nostr が空でなく `nostr-{agent}` を返す）。
    #[test]
    fn build_is_inverse_of_parse() {
        let sample = NostrFire.sample_target();
        let sid = NostrFire.build_session_id(&sample, AGENT_UUID);
        assert_eq!(sid, format!("nostr-{AGENT_UUID}"));
        assert_eq!(NostrFire.parse(&sid, AGENT_UUID), Some(sample));
    }
}
