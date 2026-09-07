use super::*;
use std::collections::HashSet;
use std::time::Duration;

use opencrab_actions::{
    accept_inbound, InboundLookups, InboundWork, PrivilegeFire, WatchAccept, WatchAllowSets,
    AGREED_IMMEDIATE_KINDS,
};

fn ev(kind: u32, tags: Vec<Vec<String>>) -> NostrEvent {
    NostrEvent {
        id: "id1".into(),
        pubkey: "aa".repeat(32),
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

#[test]
fn existing_inbound_kind_label_does_not_call_repost() {
    // 現行ラベルは kind 6 をリポストにしない（純増・既存挙動）。
    let mut e = ev(6, vec![]);
    assert_eq!(e.inbound_kind_label(), "メンション");
    e.tags = vec![vec!["e".into(), "x".into()]];
    assert_eq!(e.inbound_kind_label(), "リプライ");
}

#[test]
fn classify_interactive_immediate() {
    let p_self = vec![vec!["p".into(), SELF.to_string()]];
    let p_e_self = vec![
        vec!["e".into(), "note".into()],
        vec!["p".into(), SELF.to_string()],
    ];
    assert_eq!(
        classify_watch_event(&ev(7, vec![]), SELF, true),
        WatchForward::Immediate {
            label: "リアクション"
        }
    );
    assert_eq!(
        classify_watch_event(&ev(6, vec![]), SELF, true),
        WatchForward::Immediate {
            label: "リポスト"
        }
    );
    assert_eq!(
        classify_watch_event(&ev(16, vec![]), SELF, true),
        WatchForward::Immediate {
            label: "リポスト"
        }
    );
    assert_eq!(
        classify_watch_event(&ev(1, p_e_self), SELF, true),
        WatchForward::Immediate {
            label: "リプライ"
        }
    );
    assert_eq!(
        classify_watch_event(&ev(1, p_self.clone()), SELF, true),
        WatchForward::Immediate {
            label: "メンション"
        }
    );
    assert_eq!(
        classify_watch_event(&ev(30023, p_self), SELF, true),
        WatchForward::Immediate { label: "長文" }
    );
    assert_eq!(
        classify_watch_event(
            &ev(30023, vec![vec!["e".into(), SELF.to_string()]]),
            SELF,
            true
        ),
        WatchForward::Immediate { label: "長文" }
    );
    assert_eq!(
        classify_watch_event(
            &ev(
                30023,
                vec![vec![
                    "e".into(),
                    "noteid".into(),
                    String::new(),
                    "reply".into(),
                    SELF.to_string()
                ]]
            ),
            SELF,
            true
        ),
        WatchForward::Immediate { label: "長文" }
    );
}

#[test]
fn classify_timeline_bundle() {
    let other = ev(1, vec![vec!["p".into(), "cc".repeat(32)]]);
    assert_eq!(
        classify_watch_event(&other, SELF, true),
        WatchForward::Bundle {
            label: "メンション"
        }
    );
    assert_eq!(
        classify_watch_event(&ev(30023, vec![]), SELF, true),
        WatchForward::Bundle { label: "長文" }
    );
    assert_eq!(
        classify_watch_event(
            &ev(30023, vec![vec!["e".into(), "cc".repeat(32)]]),
            SELF,
            true
        ),
        WatchForward::Bundle { label: "長文" }
    );
    let dm = ev(4, vec![]);
    assert_eq!(classify_watch_event(&dm, SELF, true), WatchForward::Discard);
}

#[test]
fn mention_only_watch_treats_kind1_as_mention() {
    let e = ev(1, vec![]);
    assert_eq!(
        classify_watch_event(&e, SELF, false),
        WatchForward::Immediate {
            label: "メンション"
        }
    );
}

#[test]
fn bundle_flush_keeps_order() {
    let mut b = TimelineBundle::default();
    b.push(ev(1, vec![]));
    let mut e2 = ev(1, vec![]);
    e2.id = "id2".into();
    b.push(e2);
    assert_eq!(b.len(), 2);
    let taken = b.take();
    assert_eq!(taken[0].id, "id1");
    assert_eq!(taken[1].id, "id2");
    assert!(b.is_empty());
}

#[test]
fn watch_filter_rejects_garbage() {
    assert!(parse_watch_filter("").is_err());
    assert!(parse_watch_filter("[]").is_err());
    assert!(parse_watch_filter("{}").is_ok());
}

#[test]
fn watch_config_rejects_non_positive_interval() {
    let w = SessionWatchRow {
        id: 1,
        session_id: "nostr-a".into(),
        agent_id: "a".into(),
        interval_secs: 0,
        filter_json: "{}".into(),
        created_at: "t".into(),
    };
    assert!(watch_subscribe_config(&w, vec![]).is_err());
}

/// mock E2E: 束ね発火 / 即時転送 / ポリシー判定を 1 本のハーネスで固定する。
struct MockE2E {
    self_pk: String,
    beyond: bool,
    interval: u64,
    policy: String,
    owner: HashSet<String>,
    followees: HashSet<String>,
    callers: std::collections::HashMap<String, CallerIdentity>,
    allow_extra: HashSet<String>,
    bundle: TimelineBundle,
    privilege: PrivilegeFire<NostrEvent>,
    pub turns: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub dropped: Vec<String>,
    pub prepares: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub relays: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockE2E {
    fn new() -> Self {
        let turns = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prepares = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let relays = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let t = turns.clone();
        let p = prepares.clone();
        let r = relays.clone();
        let privilege = PrivilegeFire::new(move |held: Vec<(NostrEvent, CallerIdentity)>| {
            let t = t.clone();
            let p = p.clone();
            let r = r.clone();
            async move {
                let mut ids = Vec::new();
                for (e, _) in &held {
                    p.lock().unwrap().push(e.id.clone());
                    r.lock().unwrap().push(e.id.clone());
                    ids.push(format!("{}:{}", watch_kind_label(e), e.id));
                }
                t.lock()
                    .unwrap()
                    .push(format!("debounce:{}", ids.join(",")));
            }
        });
        Self {
            self_pk: SELF.into(),
            beyond: true,
            interval: 60,
            policy: "{}".into(),
            owner: HashSet::new(),
            followees: HashSet::new(),
            callers: std::collections::HashMap::new(),
            allow_extra: HashSet::new(),
            bundle: TimelineBundle::default(),
            privilege,
            turns,
            dropped: Vec::new(),
            prepares,
            relays,
        }
    }

    fn turns(&self) -> Vec<String> {
        self.turns.lock().unwrap().clone()
    }

    fn prepares(&self) -> Vec<String> {
        self.prepares.lock().unwrap().clone()
    }

    fn relays(&self) -> Vec<String> {
        self.relays.lock().unwrap().clone()
    }

    fn prepare(&self, event: &NostrEvent) {
        self.prepares.lock().unwrap().push(event.id.clone());
        self.relays.lock().unwrap().push(event.id.clone());
    }

    fn record_turn(&self, turn: String) {
        self.turns.lock().unwrap().push(turn);
    }

    fn feed(&mut self, event: NostrEvent) {
        match classify_watch_event(&event, &self.self_pk, self.beyond) {
            WatchForward::Discard => {
                self.dropped.push(format!("dm:{}", event.id));
            }
            WatchForward::Bundle { label } => {
                self.bundle.push(event);
                let _ = label;
            }
            WatchForward::Immediate { label } => {
                let key = follow_key(&event.pubkey);
                let caller = self
                    .callers
                    .get(&event.pubkey)
                    .cloned()
                    .unwrap_or(CallerIdentity::Agent);
                let resolve = |_: &str, _: &[String], _: &str| caller.clone();
                let lookups = InboundLookups {
                    resolve_caller: &resolve,
                    dm_allowed_any: &|_, _, _| true,
                    dm_allowed: &|_, _, _| true,
                    channel_whitelisted: &|_, _| true,
                };
                let empty = HashSet::new();
                let allow = WatchAllowSets {
                    followees: &self.followees,
                    owner: &self.owner,
                    co_agents: &empty,
                    trusted_users: &self.allow_extra,
                };
                let work = InboundWork {
                    event: NormalizedInboundEvent {
                        sender_id: &event.pubkey,
                        channel_id: "nostr-a",
                        guild_id: "nostr",
                    },
                    has_content: true,
                    kind_label: label,
                    author_key: &key,
                };
                let mut held = false;
                let mut admitted = false;
                let ev_hold = event.clone();
                accept_inbound(
                    &[work],
                    "",
                    &["a".into()],
                    &lookups,
                    Some(WatchAccept {
                        policy_json: &self.policy,
                        interval_secs: self.interval,
                        allow,
                        owner: &self.owner,
                        followees: &self.followees,
                        privilege: Some(&self.privilege),
                    }),
                    |_| {
                        held = true;
                        ev_hold.clone()
                    },
                    |_, _| admitted = true,
                    |_, _, _| {},
                )
                .unwrap();
                if held {
                    return;
                }
                if admitted {
                    self.prepare(&event);
                    self.record_turn(format!("immediate:{label}:{}", event.id));
                    return;
                }
                self.dropped.push(format!("allow:{}", event.id));
            }
        }
    }

    fn flush_bundle(&mut self) {
        let evs = self.bundle.take();
        if evs.is_empty() {
            return;
        }
        for e in &evs {
            self.prepare(e);
        }
        let ids: Vec<_> = evs.iter().map(|e| e.id.as_str()).collect();
        self.record_turn(format!("bundle:{}", ids.join(",")));
    }
}

#[tokio::test]
async fn e2e_bundle_fire() {
    let mut h = MockE2E::new();
    h.feed(ev(1, vec![vec!["p".into(), "cc".repeat(32)]]));
    let mut e2 = ev(1, vec![]);
    e2.id = "id2".into();
    h.feed(e2);
    assert!(h.turns().is_empty(), "束ねは flush まで発火しない");
    assert_eq!(h.bundle.len(), 2);
    h.flush_bundle();
    assert_eq!(h.turns(), vec!["bundle:id1,id2".to_string()]);
}

#[tokio::test(start_paused = true)]
async fn e2e_immediate_transfer_owner_reply() {
    let mut h = MockE2E::new();
    let owner_pk = "aa".repeat(32);
    h.owner.insert(follow_key(&owner_pk));
    h.callers.insert(owner_pk.clone(), CallerIdentity::Owner);
    let reply = ev(
        1,
        vec![
            vec!["e".into(), "note".into()],
            vec!["p".into(), SELF.into()],
        ],
    );
    h.feed(reply);
    assert_eq!(h.turns(), vec!["immediate:リプライ:id1".to_string()]);
    assert_eq!(h.prepares(), vec!["id1".to_string()]);
    assert_eq!(h.relays(), vec!["id1".to_string()]);
    assert!(h.bundle.is_empty());
    tokio::time::advance(Duration::from_secs(60)).await;
    h.flush_bundle();
    assert_eq!(
        h.prepares(),
        vec!["id1".to_string()],
        "即時は handle 時のみ prepare（時限発火で二重にしない）"
    );
    assert_eq!(h.relays(), vec!["id1".to_string()]);
    assert!(AGREED_IMMEDIATE_KINDS.contains(&"リプライ"));
}

#[tokio::test(start_paused = true)]
async fn e2e_policy_owner_repost_debounces_on_empty_policy() {
    let mut h = MockE2E::new();
    let owner_pk = "aa".repeat(32);
    h.owner.insert(follow_key(&owner_pk));
    h.callers.insert(owner_pk.clone(), CallerIdentity::Owner);
    h.feed(ev(6, vec![]));
    assert!(h.turns().is_empty());
    assert!(
        h.prepares().is_empty(),
        "束ね経路は handle で prepare しない"
    );
    assert!(h.relays().is_empty());
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !h.turns().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("watch 間隔で権限デバウンスが発火する");
    assert_eq!(h.turns(), vec!["debounce:リポスト:id1".to_string()]);
    assert_eq!(h.prepares(), vec!["id1".to_string()]);
    assert_eq!(h.relays(), vec!["id1".to_string()]);
}

#[tokio::test]
async fn e2e_policy_unallowed_is_dropped() {
    let mut h = MockE2E::new();
    h.feed(ev(7, vec![]));
    assert_eq!(h.dropped, vec!["allow:id1".to_string()]);
    assert!(h.turns().is_empty());
    assert!(h.prepares().is_empty());
}

#[tokio::test(start_paused = true)]
async fn e2e_policy_debounce_uses_class_interval_not_watch_interval() {
    let mut h = MockE2E::new();
    h.interval = 60;
    h.policy = serde_json::json!({
        "Owner": { "debounce_secs": 0, "immediate": ["リプライ"] },
        "CoAgent": { "debounce_secs": 0, "immediate": ["リプライ"] },
        "TrustedUser": { "debounce_secs": 120, "immediate": [] },
        "Agent": { "debounce_secs": 300, "immediate": [] },
    })
    .to_string();
    let author = "aa".repeat(32);
    h.allow_extra.insert(follow_key(&author));
    h.callers.insert(author, CallerIdentity::Agent);
    h.feed(ev(6, vec![]));
    assert!(h.turns().is_empty());
    assert!(h.prepares().is_empty());
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(
        h.turns().is_empty(),
        "watch interval 60s では発火しない（権限間隔 300s）"
    );
    tokio::time::advance(Duration::from_secs(240)).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !h.turns().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("権限間隔 300s で発火する");
    assert_eq!(h.turns(), vec!["debounce:リポスト:id1".to_string()]);
    assert_eq!(h.prepares(), vec!["id1".to_string()]);
    assert_eq!(h.relays(), vec!["id1".to_string()]);
}
