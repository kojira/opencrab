//! Core Nostr inbound adapter。
//!
//! 元栓（allow-set・DM・自己投稿）を `accept_inbound` の記録 callback 前に評価する。
//! default / watch ともこの 1 口。`route` は輸送の正（再分類しない）。

use std::collections::HashSet;

use opencrab_actions::{
    accept_inbound, AdmittedInbound, InboundDrop, InboundLookups, InboundWork,
    NormalizedInboundEvent, WatchAccept, WatchAllowSets,
};

use crate::event::NostrEvent;
use crate::pubkey::follow_key;
use crate::watch::{classify_watch_event, WatchForward};

/// 輸送時機。gateway が確定した `route` をそのまま使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressRoute {
    Default,
    Immediate,
    Bundle,
}

impl IngressRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Immediate => "immediate",
            Self::Bundle => "bundle",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "default" => Some(Self::Default),
            "immediate" => Some(Self::Immediate),
            "bundle" => Some(Self::Bundle),
            _ => None,
        }
    }
}

/// 記録 callback 前に落とす理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Dm,
    SelfPost,
    AllowSet,
}

/// 元栓の許可源（フォロイー ∪ owner ∪ co_agent ∪ trusted_users）。
#[derive(Debug, Default, Clone)]
pub struct AllowSources {
    pub followees: HashSet<String>,
    pub owner: HashSet<String>,
    pub co_agents: HashSet<String>,
    pub trusted_users: HashSet<String>,
}

impl AllowSources {
    pub fn is_allowed(&self, author_key: &str) -> bool {
        self.followees.contains(author_key)
            || self.owner.contains(author_key)
            || self.co_agents.contains(author_key)
            || self.trusted_users.contains(author_key)
    }

    pub fn as_watch_allow(&self) -> WatchAllowSets<'_> {
        WatchAllowSets {
            followees: &self.followees,
            owner: &self.owner,
            co_agents: &self.co_agents,
            trusted_users: &self.trusted_users,
        }
    }
}

/// `route` を輸送の正とする。未指定のときだけ `classify_watch_event` で補う。
pub fn transport_route(
    provided: Option<IngressRoute>,
    event: &NostrEvent,
    self_pubkey: &str,
    beyond_self: bool,
    default_lane: bool,
) -> IngressRoute {
    if default_lane {
        return IngressRoute::Default;
    }
    if let Some(route) = provided {
        return route;
    }
    match classify_watch_event(event, self_pubkey, beyond_self) {
        WatchForward::Discard => IngressRoute::Immediate,
        WatchForward::Immediate { .. } => IngressRoute::Immediate,
        WatchForward::Bundle { .. } => IngressRoute::Bundle,
    }
}

/// 記録 callback 前の元栓。DB も accept_inbound も触らない。
pub fn pre_record_drop(
    event: &NostrEvent,
    self_pubkey: &str,
    allow: &AllowSources,
) -> Option<DropReason> {
    if event.is_dm() {
        return Some(DropReason::Dm);
    }
    if event.pubkey == self_pubkey {
        return Some(DropReason::SelfPost);
    }
    let author_key = follow_key(&event.pubkey);
    if !allow.is_allowed(&author_key) {
        return Some(DropReason::AllowSet);
    }
    None
}

/// default / watch とも `accept_inbound` exact 1 回。
#[allow(clippy::too_many_arguments)]
pub fn accept_nostr_inbound<T: Send + 'static>(
    event: &NostrEvent,
    agent_id: &str,
    session_id: &str,
    owner_id: &str,
    kind_label: &str,
    lookups: &InboundLookups<'_>,
    watch: Option<WatchAccept<'_, T>>,
    take_hold: impl FnMut(usize) -> T,
    on_admitted: impl FnMut(usize, &AdmittedInbound),
    on_run: impl FnMut(usize, &AdmittedInbound, &[usize]),
) -> Result<(), InboundDrop> {
    let key = follow_key(&event.pubkey);
    let work = [InboundWork {
        event: NormalizedInboundEvent {
            sender_id: &event.pubkey,
            channel_id: session_id,
            guild_id: "nostr",
        },
        has_content: true,
        kind_label,
        author_key: &key,
    }];
    accept_inbound(
        &work,
        owner_id,
        &[agent_id.to_string()],
        lookups,
        watch,
        take_hold,
        on_admitted,
        on_run,
    )
}

const V1_PREFIX: &str = "[NOSTRGATE/V1 ";

/// 版付きアンカー。key 集合・型・`route` を検証する。不正は `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V1Anchor {
    pub beyond_self: bool,
    pub bundle_id: Option<String>,
    pub count: Option<u32>,
    pub event_id: String,
    pub has_e: bool,
    pub index: Option<u32>,
    pub kind: u32,
    pub p_self: bool,
    pub route: IngressRoute,
    pub watch_id: Option<i64>,
}

pub fn parse_v1_anchor(text: &str) -> Option<V1Anchor> {
    let line = text.lines().next()?.trim();
    let rest = line.strip_prefix(V1_PREFIX)?;
    let json = rest.strip_suffix(']')?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;
    const KEYS: [&str; 10] = [
        "beyond_self",
        "bundle_id",
        "count",
        "event_id",
        "has_e",
        "index",
        "kind",
        "p_self",
        "route",
        "watch_id",
    ];
    if obj.len() != KEYS.len() {
        return None;
    }
    for key in KEYS {
        if !obj.contains_key(key) {
            return None;
        }
    }
    let bool_field = |k: &str| obj.get(k)?.as_bool();
    let hex64 = |k: &str| {
        let s = obj.get(k)?.as_str()?;
        if s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()) {
            Some(s.to_ascii_lowercase())
        } else {
            None
        }
    };
    let opt_u32 = |k: &str| match obj.get(k)? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::Number(n) => Some(Some(n.as_u64()? as u32)),
        _ => None,
    };
    let opt_i64 = |k: &str| match obj.get(k)? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::Number(n) => Some(Some(n.as_i64()?)),
        _ => None,
    };
    let opt_string = |k: &str| match obj.get(k)? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::String(s) => Some(Some(s.clone())),
        _ => None,
    };
    let kind = obj.get("kind")?.as_u64()? as u32;
    let route = IngressRoute::parse(obj.get("route")?.as_str()?)?;
    Some(V1Anchor {
        beyond_self: bool_field("beyond_self")?,
        bundle_id: opt_string("bundle_id")?,
        count: opt_u32("count")?,
        event_id: hex64("event_id")?,
        has_e: bool_field("has_e")?,
        index: opt_u32("index")?,
        kind,
        p_self: bool_field("p_self")?,
        route,
        watch_id: opt_i64("watch_id")?,
    })
}

/// 本文から JSON アンカー行を除いた renderer 生本文。
pub fn history_body_without_anchor(text: &str) -> &str {
    let Some(first) = text.lines().next() else {
        return text;
    };
    if first.starts_with(V1_PREFIX) {
        let rest = text.get(first.len()..).unwrap_or("");
        return rest.strip_prefix('\n').unwrap_or(rest);
    }
    text
}

/// V3 said を record 前に落とす理由。不正アンカーは `BadAnchor`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitSaidError {
    BadAnchor,
    Drop(DropReason),
}

/// 版付きアンカーを検証し、DM / 自己投稿 / allow-set を record 前に落とす。
///
/// アンカーは分類にだけ使う。caller / allow-set の根拠にはしない。
pub fn admit_nostr_said(
    text: &str,
    author_id: &str,
    self_pubkey: &str,
    allow: &AllowSources,
) -> Result<V1Anchor, AdmitSaidError> {
    let Some(anchor) = parse_v1_anchor(text) else {
        return Err(AdmitSaidError::BadAnchor);
    };
    if matches!(anchor.kind, 4 | 1059) {
        return Err(AdmitSaidError::Drop(DropReason::Dm));
    }
    if author_id == self_pubkey {
        return Err(AdmitSaidError::Drop(DropReason::SelfPost));
    }
    if !allow.is_allowed(&follow_key(author_id)) {
        return Err(AdmitSaidError::Drop(DropReason::AllowSet));
    }
    Ok(anchor)
}

/// agent 単位の allow-set と self pubkey。更新失敗は差し替えない（前回値保持）。
#[derive(Debug, Default, Clone)]
pub struct AllowSetStore {
    allow: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, AllowSources>>>,
    selves: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>,
}

impl AllowSetStore {
    pub fn replace_allow(&self, agent_id: &str, sources: AllowSources) {
        self.allow
            .write()
            .expect("allow-set store")
            .insert(agent_id.to_string(), sources);
    }

    pub fn set_self_pubkey(&self, agent_id: &str, self_pubkey: String) {
        self.selves
            .write()
            .expect("allow-set store")
            .insert(agent_id.to_string(), self_pubkey);
    }

    pub fn remove(&self, agent_id: &str) {
        self.allow.write().expect("allow-set store").remove(agent_id);
        self.selves
            .write()
            .expect("allow-set store")
            .remove(agent_id);
    }

    pub fn get_allow(&self, agent_id: &str) -> Option<AllowSources> {
        self.allow.read().expect("allow-set store").get(agent_id).cloned()
    }

    pub fn self_pubkey(&self, agent_id: &str) -> Option<String> {
        self.selves
            .read()
            .expect("allow-set store")
            .get(agent_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::{CallerIdentity, PrivilegeFire, WatchAccept};

    fn ev(kind: u32, pubkey: &str, tags: Vec<Vec<String>>) -> NostrEvent {
        NostrEvent {
            id: "aa".repeat(32),
            pubkey: pubkey.to_string(),
            npub: None,
            note_id: Some("note1x".into()),
            author_name: None,
            created_at: 1,
            kind,
            content: "hi".into(),
            tags,
        }
    }

    const SELF: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const OTHER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn allow_other() -> AllowSources {
        let mut a = AllowSources::default();
        a.followees.insert(follow_key(OTHER));
        a
    }

    #[test]
    fn pre_record_drops_dm_before_accept() {
        let event = ev(4, OTHER, vec![]);
        assert_eq!(
            pre_record_drop(&event, SELF, &allow_other()),
            Some(DropReason::Dm)
        );
        let event = ev(1059, OTHER, vec![]);
        assert_eq!(
            pre_record_drop(&event, SELF, &allow_other()),
            Some(DropReason::Dm)
        );
    }

    #[test]
    fn pre_record_drops_self_post() {
        let event = ev(1, SELF, vec![]);
        let mut allow = AllowSources::default();
        allow.followees.insert(follow_key(SELF));
        assert_eq!(
            pre_record_drop(&event, SELF, &allow),
            Some(DropReason::SelfPost)
        );
    }

    #[test]
    fn pre_record_drops_unallowed_author() {
        let event = ev(1, OTHER, vec![]);
        assert_eq!(
            pre_record_drop(&event, SELF, &AllowSources::default()),
            Some(DropReason::AllowSet)
        );
    }

    #[test]
    fn pre_record_admits_followee() {
        let event = ev(1, OTHER, vec![]);
        assert_eq!(pre_record_drop(&event, SELF, &allow_other()), None);
    }

    #[test]
    fn transport_route_uses_provided_route_as_truth() {
        let event = ev(1, OTHER, vec![vec!["p".into(), "cc".repeat(32)]]);
        assert_eq!(
            transport_route(Some(IngressRoute::Immediate), &event, SELF, true, false),
            IngressRoute::Immediate
        );
        assert_eq!(
            transport_route(Some(IngressRoute::Bundle), &event, SELF, true, false),
            IngressRoute::Bundle
        );
    }

    #[test]
    fn transport_route_default_lane_is_default() {
        let event = ev(1, OTHER, vec![]);
        assert_eq!(
            transport_route(None, &event, SELF, true, true),
            IngressRoute::Default
        );
    }

    #[test]
    fn classify_watch_maps_dm_to_immediate_transport() {
        let event = ev(4, OTHER, vec![]);
        assert_eq!(
            classify_watch_event(&event, SELF, true),
            WatchForward::Discard
        );
        assert_eq!(
            transport_route(None, &event, SELF, true, false),
            IngressRoute::Immediate
        );
    }

    #[test]
    fn classify_watch_immediate_and_bundle() {
        let mention = ev(1, OTHER, vec![vec!["p".into(), SELF.to_string()]]);
        assert_eq!(
            transport_route(None, &mention, SELF, true, false),
            IngressRoute::Immediate
        );
        let timeline = ev(1, OTHER, vec![vec!["p".into(), "cc".repeat(32)]]);
        assert_eq!(
            transport_route(None, &timeline, SELF, true, false),
            IngressRoute::Bundle
        );
    }

    #[test]
    fn accept_default_is_exact_one_accept_inbound() {
        let event = ev(1, OTHER, vec![]);
        let resolve = |_: &str, _: &[String], _: &str| CallerIdentity::Agent;
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let mut admitted = 0usize;
        let mut runs = 0usize;
        accept_nostr_inbound::<()>(
            &event,
            "a1",
            "nostr-a1",
            "",
            "メンション",
            &lookups,
            None,
            |_| (),
            |_, _| admitted += 1,
            |_, _, _| runs += 1,
        )
        .unwrap();
        assert_eq!(admitted, 1);
        assert_eq!(runs, 1);
    }

    #[tokio::test]
    async fn accept_watch_immediate_uses_watch_accept() {
        let event = ev(1, OTHER, vec![vec!["p".into(), SELF.to_string()]]);
        let allow = allow_other();
        let resolve = |_: &str, _: &[String], _: &str| CallerIdentity::Owner;
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| false,
            dm_allowed: &|_, _, _| false,
            channel_whitelisted: &|_, _| false,
        };
        let privilege = PrivilegeFire::new(|_: Vec<(NostrEvent, CallerIdentity)>| async {});
        let mut admitted = 0usize;
        let mut runs = 0usize;
        let mut held = false;
        let hold_event = event.clone();
        accept_nostr_inbound(
            &event,
            "a1",
            "nostr-a1",
            "",
            "リプライ",
            &lookups,
            Some(WatchAccept {
                policy_json: r#"{"Owner":{"debounce_secs":0,"immediate":["リプライ"]},"CoAgent":{"debounce_secs":0,"immediate":["リプライ"]},"TrustedUser":{"debounce_secs":0,"immediate":[]},"Agent":{"debounce_secs":0,"immediate":[]}}"#,
                interval_secs: 60,
                allow: allow.as_watch_allow(),
                owner: &allow.owner,
                followees: &allow.followees,
                privilege: Some(&privilege),
            }),
            |_| {
                held = true;
                hold_event.clone()
            },
            |_, _| admitted += 1,
            |_, _, _| runs += 1,
        )
        .unwrap();
        assert!(!held);
        assert_eq!(admitted, 1);
        assert_eq!(runs, 1);
    }

    #[test]
    fn v1_anchor_rejects_missing_key_and_bad_route() {
        assert!(parse_v1_anchor("hello").is_none());
        let bad_route = format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":false,\"bundle_id\":null,\"count\":null,\"event_id\":\"{}\",\"has_e\":false,\"index\":null,\"kind\":1,\"p_self\":true,\"route\":\"discard\",\"watch_id\":null}}]",
            "aa".repeat(32)
        );
        assert!(parse_v1_anchor(&bad_route).is_none());
        let extra = format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":false,\"bundle_id\":null,\"count\":null,\"event_id\":\"{}\",\"has_e\":false,\"index\":null,\"kind\":1,\"p_self\":true,\"route\":\"default\",\"watch_id\":null,\"extra\":1}}]",
            "aa".repeat(32)
        );
        assert!(parse_v1_anchor(&extra).is_none());
    }

    #[test]
    fn v1_anchor_accepts_canonical_immediate() {
        let line = format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":false,\"bundle_id\":null,\"count\":null,\"event_id\":\"{}\",\"has_e\":true,\"index\":null,\"kind\":1,\"p_self\":true,\"route\":\"immediate\",\"watch_id\":17}}]\nhello",
            "aa".repeat(32)
        );
        let parsed = parse_v1_anchor(&line).unwrap();
        assert_eq!(parsed.route, IngressRoute::Immediate);
        assert_eq!(parsed.watch_id, Some(17));
        assert_eq!(parsed.kind, 1);
        assert_eq!(parsed.event_id, "aa".repeat(32));
        assert_eq!(history_body_without_anchor(&line), "hello");
    }

    fn v1_line(kind: u32, route: &str) -> String {
        format!(
            "[NOSTRGATE/V1 {{\"beyond_self\":false,\"bundle_id\":null,\"count\":null,\"event_id\":\"{}\",\"has_e\":false,\"index\":null,\"kind\":{kind},\"p_self\":true,\"route\":\"{route}\",\"watch_id\":null}}]\nhello",
            "aa".repeat(32)
        )
    }

    #[test]
    fn admit_said_drops_dm_self_and_unallowed_before_record() {
        let allow = allow_other();
        let dm = v1_line(4, "immediate");
        assert_eq!(
            admit_nostr_said(&dm, OTHER, SELF, &allow),
            Err(AdmitSaidError::Drop(DropReason::Dm))
        );
        let note = v1_line(1, "default");
        assert_eq!(
            admit_nostr_said(&note, SELF, SELF, &allow),
            Err(AdmitSaidError::Drop(DropReason::SelfPost))
        );
        assert_eq!(
            admit_nostr_said(&note, &"dd".repeat(32), SELF, &allow),
            Err(AdmitSaidError::Drop(DropReason::AllowSet))
        );
        assert!(admit_nostr_said(&note, OTHER, SELF, &allow).is_ok());
        assert_eq!(
            admit_nostr_said("not-an-anchor", OTHER, SELF, &allow),
            Err(AdmitSaidError::BadAnchor)
        );
    }

    #[test]
    fn allow_set_store_keeps_last_value() {
        let store = AllowSetStore::default();
        store.replace_allow("a1", allow_other());
        store.set_self_pubkey("a1", SELF.to_string());
        assert!(store.get_allow("a1").unwrap().is_allowed(&follow_key(OTHER)));
        assert_eq!(store.self_pubkey("a1").as_deref(), Some(SELF));
        store.remove("a1");
        assert!(store.get_allow("a1").is_none());
    }
}
