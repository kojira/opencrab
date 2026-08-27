//! `v3_shadow`: watch parse/分類を gateway とメモリ内で照合する。
//!
//! Binding PUT / said / say は行わない（DESIGN-NOSTRGATE §7.2）。
//! 本番 UDS への接続・hello・bind ack・live 占有はしない。

use opencrab_nostr_gateway::map::{
    classify_route, parse_watch_line as parse_gateway_line, Lane, Route, WatchEvent,
};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::event::{parse_watch_line, NostrEvent};
use crate::watch::{classify_watch_event, WatchForward};

pub fn config_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hex_lower(&hash)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 同一 JSONL 行を legacy / gateway の両 parser で読み、食い違いを残す。
pub fn compare_parse(line: &str) {
    let legacy = parse_watch_line(line);
    let gateway = parse_gateway_line(line);
    match (legacy.as_ref(), gateway.as_ref()) {
        (Some(a), Some(b)) if parse_fields_match(a, b) => {
            debug!(event_id = %a.id, "v3_shadow parse agree");
        }
        (None, None) => {
            debug!("v3_shadow parse agree (both drop)");
        }
        _ => {
            warn!(
                legacy = legacy.is_some(),
                gateway = gateway.is_some(),
                "v3_shadow parse mismatch"
            );
        }
    }
}

fn parse_fields_match(a: &NostrEvent, b: &WatchEvent) -> bool {
    a.id == b.id
        && a.pubkey == b.pubkey
        && a.kind == b.kind
        && a.created_at == b.created_at
        && a.content == b.content
        && a.tags == b.tags
}

/// legacy `classify_watch_event` と gateway `classify_route` を同じ event で照合する。
///
/// default lane は gateway が常に `Route::Default`。watch の DM は legacy Discard /
/// gateway Immediate（設計どおり一致扱い）。
pub fn compare_classify(
    event: &NostrEvent,
    self_pubkey: &str,
    beyond_self: bool,
    watch_id: Option<i64>,
) {
    let gw_event = to_watch_event(event);
    let lane = match watch_id {
        Some(id) => Lane::watch(id),
        None => Lane::default_lane(),
    };
    let gateway = classify_route(&gw_event, self_pubkey, beyond_self, &lane);
    if watch_id.is_none() {
        if gateway == Route::Default {
            debug!(event_id = %event.id, "v3_shadow classify agree (default)");
        } else {
            warn!(event_id = %event.id, ?gateway, "v3_shadow classify mismatch (default lane)");
        }
        return;
    }
    let legacy = classify_watch_event(event, self_pubkey, beyond_self);
    if routes_agree(&legacy, gateway) {
        debug!(event_id = %event.id, ?legacy, ?gateway, "v3_shadow classify agree");
    } else {
        warn!(event_id = %event.id, ?legacy, ?gateway, "v3_shadow classify mismatch");
    }
}

fn to_watch_event(event: &NostrEvent) -> WatchEvent {
    WatchEvent {
        id: event.id.clone(),
        pubkey: event.pubkey.clone(),
        npub: event.npub.clone(),
        note_id: event.note_id.clone(),
        created_at: event.created_at,
        kind: event.kind,
        content: event.content.clone(),
        tags: event.tags.clone(),
    }
}

fn routes_agree(legacy: &WatchForward, gateway: Route) -> bool {
    matches!(
        (legacy, gateway),
        (WatchForward::Discard, Route::Immediate)
            | (WatchForward::Immediate { .. }, Route::Immediate)
            | (WatchForward::Bundle { .. }, Route::Bundle)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: u32, tags: Vec<Vec<String>>) -> NostrEvent {
        NostrEvent {
            id: "11".repeat(32),
            pubkey: "22".repeat(32),
            npub: None,
            note_id: None,
            author_name: None,
            created_at: 1,
            kind,
            content: "hi".into(),
            tags,
        }
    }

    #[test]
    fn dm_discard_agrees_with_gateway_immediate() {
        let event = ev(4, vec![]);
        let self_pk = "aa".repeat(32);
        compare_classify(&event, &self_pk, false, Some(7));
        let gw = classify_route(&to_watch_event(&event), &self_pk, false, &Lane::watch(7));
        assert_eq!(gw, Route::Immediate);
        assert!(routes_agree(
            &classify_watch_event(&event, &self_pk, false),
            gw
        ));
    }

    #[test]
    fn mention_to_self_is_immediate_on_both() {
        let self_pk = "aa".repeat(32);
        let event = ev(1, vec![vec!["p".into(), self_pk.clone()]]);
        let gw = classify_route(&to_watch_event(&event), &self_pk, false, &Lane::watch(1));
        assert_eq!(gw, Route::Immediate);
        assert!(routes_agree(
            &classify_watch_event(&event, &self_pk, false),
            gw
        ));
    }

    #[test]
    fn timeline_kind1_is_bundle_on_both() {
        let self_pk = "aa".repeat(32);
        let event = ev(1, vec![vec!["e".into(), "33".repeat(32)]]);
        let gw = classify_route(&to_watch_event(&event), &self_pk, true, &Lane::watch(1));
        assert_eq!(gw, Route::Bundle);
        assert!(routes_agree(
            &classify_watch_event(&event, &self_pk, true),
            gw
        ));
    }

    #[test]
    fn default_lane_is_always_default() {
        let self_pk = "aa".repeat(32);
        let event = ev(1, vec![vec!["p".into(), self_pk.clone()]]);
        let gw = classify_route(
            &to_watch_event(&event),
            &self_pk,
            false,
            &Lane::default_lane(),
        );
        assert_eq!(gw, Route::Default);
    }

    #[test]
    fn parse_agree_on_same_object() {
        let line =
            r#"{"id":"abc","pubkey":"dead","created_at":1,"kind":1,"content":"x","tags":[]}"#;
        compare_parse(line);
        assert!(parse_watch_line(line).is_some());
        assert!(parse_gateway_line(line).is_some());
    }

    #[test]
    fn digest_is_sha256_lowerhex() {
        let d = config_digest(b"abc");
        assert_eq!(d.len(), 64);
        assert!(d.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(
            d,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
