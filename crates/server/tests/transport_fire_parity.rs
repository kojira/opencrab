//! #628 段階2 パリティテスト: 旧 `SessionFireTarget`（db 層の enum）と、新しい各 transport の
//! `TransportFire` descriptor が **同一入力で完全に等価**なことを担保する。
//!
//! 旧テスト（`crates/db/.../session_heartbeat.rs`）の全ケースを新旧両実装へ同一入力で食わせる:
//! 正常 2 / fail-closed 4（`web-` / `heartbeat-` / `agent-msg-` / 非数値 guild・channel）/
//! 別 agent_id / round-trip 両方向。入力集合を絞ると等価性の証明にならないので全ケースを回す。
//!
//! 旧 enum を削除する段階4 まで、このテストがリファクタの純粋性（挙動差ゼロ）を保護する。

#![cfg(feature = "discord")]

use opencrab_actions::{FireTarget, TransportFire};
use opencrab_db::queries::{resolve_session_fire_target, SessionFireTarget};
use opencrab_discord::DiscordFire;
use opencrab_nostr::NostrFire;

const AGENT_UUID: &str = "6b79ac3a-7f17-4618-a827-5bda992a3698";

/// 旧 enum を新 `FireTarget` へ写像する（比較のため）。
fn old_to_new(old: &SessionFireTarget) -> FireTarget {
    match old {
        SessionFireTarget::NostrBroadcast => FireTarget {
            kind: "nostr",
            channel_id: String::new(),
            guild_id: String::new(),
            route: String::new(),
        },
        SessionFireTarget::DiscordChannel {
            guild_id,
            channel_id,
        } => FireTarget {
            kind: "discord",
            channel_id: channel_id.clone(),
            guild_id: guild_id.clone(),
            route: String::new(),
        },
    }
}

/// 新 descriptor 群を登録簿順（Discord → Nostr）に first-match で解決する。
fn new_resolve(session_id: &str, agent_id: &str) -> Option<FireTarget> {
    DiscordFire
        .parse(session_id, agent_id)
        .or_else(|| NostrFire.parse(session_id, agent_id))
}

/// 段階2 の入力集合（旧テストの全ケース）。`(session_id, agent_id)`。
fn parity_inputs() -> Vec<(String, &'static str)> {
    vec![
        // 正常 2。
        (format!("nostr-{AGENT_UUID}"), AGENT_UUID),
        (format!("discord-{AGENT_UUID}-1001-2002"), AGENT_UUID),
        // fail-closed 4。
        (format!("web-{AGENT_UUID}"), AGENT_UUID),
        (format!("heartbeat-{AGENT_UUID}-2002"), AGENT_UUID),
        (format!("agent-msg-{AGENT_UUID}"), AGENT_UUID),
        (format!("discord-{AGENT_UUID}-guild-chan"), AGENT_UUID),
        // 別 agent_id（保存済み agent_id で剥がすので剥がれない）。
        (format!("discord-{AGENT_UUID}-1001-2002"), "other-agent"),
    ]
}

/// parse（session_id → 発火先）が新旧で完全一致する（全ケース）。
#[test]
fn parse_matches_old_enum_for_all_inputs() {
    for (sid, agent) in parity_inputs() {
        let old = resolve_session_fire_target(&sid, agent).map(|o| old_to_new(&o));
        let new = new_resolve(&sid, agent);
        assert_eq!(old, new, "parse 不一致: session_id={sid} agent={agent}");
    }
}

/// build（発火先 → session_id）が新旧で完全一致する（両変種）。
#[test]
fn build_matches_old_enum_both_variants() {
    // Nostr。
    let old_nostr = SessionFireTarget::NostrBroadcast.channel_session_id(AGENT_UUID);
    let new_nostr = NostrFire.build_session_id(&NostrFire.sample_target(), AGENT_UUID);
    assert_eq!(old_nostr, format!("nostr-{AGENT_UUID}"));
    assert_eq!(old_nostr, new_nostr);

    // Discord。
    let old_discord = SessionFireTarget::DiscordChannel {
        guild_id: "1001".to_string(),
        channel_id: "2002".to_string(),
    }
    .channel_session_id(AGENT_UUID);
    let new_discord = DiscordFire.build_session_id(&DiscordFire.sample_target(), AGENT_UUID);
    assert_eq!(old_discord, format!("discord-{AGENT_UUID}-1001-2002"));
    assert_eq!(old_discord, new_discord);
}

/// round-trip 両方向が新旧で一致する（#508: Nostr が空でなく `nostr-{agent}` を返すのが要点）。
#[test]
fn round_trip_both_directions_match_old() {
    for old in [
        SessionFireTarget::NostrBroadcast,
        SessionFireTarget::DiscordChannel {
            guild_id: "1001".to_string(),
            channel_id: "2002".to_string(),
        },
    ] {
        // 旧: build → parse → 同じ enum。
        let sid = old.channel_session_id(AGENT_UUID);
        assert_eq!(resolve_session_fire_target(&sid, AGENT_UUID), Some(old.clone()));

        // 新: 同じ session_id から parse → build で戻る。
        let new = new_resolve(&sid, AGENT_UUID).expect("新実装が parse できない");
        assert_eq!(new, old_to_new(&old), "round-trip parse 不一致: {sid}");
        let rebuilt = match new.kind {
            "discord" => DiscordFire.build_session_id(&new, AGENT_UUID),
            "nostr" => NostrFire.build_session_id(&new, AGENT_UUID),
            other => panic!("未知 kind: {other}"),
        };
        assert_eq!(rebuilt, sid, "round-trip build 不一致: {sid}");
    }
}
