use super::*;
use crate::watch::plan_watch_args;

#[test]
fn watches_present_still_spawns_mention_keyword_lane() {
    let self_pk = "aa".repeat(32);
    let cfg = InstanceConfig {
        relays: vec!["wss://example.invalid".into()],
        filter: WatchFilter::default(),
        self_pubkey: self_pk.clone(),
        name: Some("crab".into()),
        watches: vec![WatchPlacement {
            id: 3,
            interval_secs: 120,
            max_items: crate::config::DEFAULT_BUNDLE_MAX_ITEMS,
            filter: WatchFilter {
                authors: vec!["npub1watched".into()],
                ..WatchFilter::default()
            },
            filter_json: None,
        }],
        delivery_mode: None,
    };
    let planned = plan_lane_spawns(&cfg);
    assert_eq!(planned.len(), 2, "mention + watch");
    assert_eq!(planned[0].lane, Lane::default_lane());
    assert!(planned[0].watch.is_none());
    let mention_args = plan_watch_args(&cfg.relays, &planned[0].filter);
    assert_eq!(mention_args[0], "watch");
    assert!(
        mention_args.contains(&"--keyword=crab".to_string()),
        "name keyword missing: {mention_args:?}"
    );
    assert!(
        mention_args.contains(&format!("--npub={self_pk}")),
        "p-tag target missing: {mention_args:?}"
    );
    assert!(
        !mention_args.contains(&format!("--keyword={self_pk}")),
        "hex pubkey must not be a keyword: {mention_args:?}"
    );
    assert!(
        mention_args.contains(&"--kind=1".to_string()),
        "{mention_args:?}"
    );
    assert!(
        mention_args.contains(&"--kind=7".to_string()),
        "{mention_args:?}"
    );
    assert!(
        !mention_args.iter().any(|a| a.starts_with("--author=")),
        "mention lane must not take watch authors: {mention_args:?}"
    );
    let keyword_count = mention_args
        .iter()
        .filter(|a| a.starts_with("--keyword="))
        .count();
    assert_eq!(keyword_count, 1, "{mention_args:?}");
    assert_eq!(planned[1].lane, Lane::watch(3));
    let watch_args = plan_watch_args(&cfg.relays, &planned[1].filter);
    assert!(
        watch_args.contains(&"--author=npub1watched".to_string()),
        "{watch_args:?}"
    );
    assert!(
        !watch_args.iter().any(|a| a.starts_with("--keyword=")),
        "timeline lane must not inherit mention keywords: {watch_args:?}"
    );
    assert!(
        !watch_args.iter().any(|a| a.starts_with("--npub=")),
        "timeline lane must not inherit mention npub: {watch_args:?}"
    );
}

#[test]
fn claim_or_skip_dedups_hex_ids_and_counts() {
    let metrics = SaidMetrics::default();
    let seen = SeenEvents::new(Duration::from_secs(600));
    let id = "aa".repeat(32);
    assert!(claim_or_skip(&seen, &metrics, &id), "初回は送る");
    assert!(!claim_or_skip(&seen, &metrics, &id), "二度目は重複で落とす");
    assert_eq!(metrics.deduped.load(Ordering::Relaxed), 1);
    // 別 event_id は独立に送れる。
    assert!(claim_or_skip(&seen, &metrics, &"bb".repeat(32)));
    assert_eq!(metrics.deduped.load(Ordering::Relaxed), 1);
}

#[test]
fn claim_or_skip_passes_non_hex_without_dedup() {
    let metrics = SaidMetrics::default();
    let seen = SeenEvents::new(Duration::from_secs(600));
    // 非 hex は dedup キーにできないので常に通す（下流 map_event が落とす）。
    assert!(claim_or_skip(&seen, &metrics, "not-hex"));
    assert!(claim_or_skip(&seen, &metrics, "not-hex"));
    assert_eq!(metrics.deduped.load(Ordering::Relaxed), 0);
}

fn watch_event(kind: u32, tags: Vec<Vec<String>>) -> WatchEvent {
    WatchEvent {
        id: "aa".repeat(32),
        pubkey: "22".repeat(32),
        npub: None,
        note_id: Some("note1abc".into()),
        created_at: 1,
        kind,
        content: "hi".into(),
        tags,
    }
}

// Defect A（QC #10）: watch 車線は自分宛て #p を default 車線へ譲る。default 車線と
// 他人宛て/#p 無しは譲らない。
#[test]
fn watch_lane_defers_self_p_tag_only() {
    let self_pk = "11".repeat(32);
    let self_mention = watch_event(1, vec![vec!["p".into(), self_pk.clone()]]);
    // watch 車線 × 自分宛て → 譲る（default 車線の即時処理へ一本化）。
    assert!(should_defer_to_default_lane(
        &Lane::watch(4),
        &self_mention,
        &self_pk
    ));
    // default 車線は決して譲らない（自分がここで即時処理する）。
    assert!(!should_defer_to_default_lane(
        &Lane::default_lane(),
        &self_mention,
        &self_pk
    ));
    // watch 車線 × 他人宛て → 譲らない（timeline の担当）。
    let other = watch_event(1, vec![vec!["p".into(), "99".repeat(32)]]);
    assert!(!should_defer_to_default_lane(
        &Lane::watch(4),
        &other,
        &self_pk
    ));
    // watch 車線 × #p 無しリプライ → 譲らない（default 車線の --npub では拾えない）。
    let e_only = watch_event(1, vec![vec!["e".into(), "bb".repeat(32)]]);
    assert!(!should_defer_to_default_lane(
        &Lane::watch(4),
        &e_only,
        &self_pk
    ));
}

#[test]
fn dedup_ttl_covers_max_watch_interval() {
    let mut cfg = InstanceConfig {
        relays: vec!["wss://example.invalid".into()],
        filter: WatchFilter::default(),
        self_pubkey: "aa".repeat(32),
        name: None,
        watches: vec![],
        delivery_mode: None,
    };
    // watch 無しは下限。
    assert_eq!(dedup_ttl(&cfg), DEDUP_TTL_FLOOR);
    // 大きい interval は 2 倍＋余裕で下限を超える。
    cfg.watches = vec![WatchPlacement {
        id: 1,
        interval_secs: 3600,
        max_items: crate::config::DEFAULT_BUNDLE_MAX_ITEMS,
        filter: WatchFilter::default(),
        filter_json: None,
    }];
    let ttl = dedup_ttl(&cfg);
    assert!(
        ttl >= Duration::from_secs(3600 * 2),
        "TTL は最大 interval の 2 倍以上: {ttl:?}"
    );
}

#[test]
fn store_error_and_bad_request_are_counted() {
    let metrics = SaidMetrics::default();
    record_said_outcome(
        &metrics,
        "o1",
        &SaidOutcome::WireErr {
            code: "store_error".into(),
            detail: None,
        },
    );
    record_said_outcome(
        &metrics,
        "o2",
        &SaidOutcome::WireErr {
            code: "bad_request".into(),
            detail: Some("anchor".into()),
        },
    );
    record_said_outcome(
        &metrics,
        "o3",
        &SaidOutcome::WireErr {
            code: "store_error".into(),
            detail: None,
        },
    );
    record_said_outcome(&metrics, "o4", &SaidOutcome::NotAdmitted);
    assert_eq!(metrics.store_error.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.bad_request.load(Ordering::Relaxed), 1);
}

fn hex_id(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

fn timeline_event(n: u8, created_at: i64) -> WatchEvent {
    WatchEvent {
        id: hex_id(n),
        pubkey: hex_id(0xaa),
        npub: None,
        note_id: None,
        created_at,
        kind: 1,
        content: format!("e{n}"),
        tags: vec![],
    }
}

fn prepared_manifest(watch_id: i64, events: &[WatchEvent]) -> (String, Vec<String>, u32) {
    let ids: Vec<String> = events
        .iter()
        .map(|e| crate::map::normalize_author_id(&e.id).expect("hex id"))
        .collect();
    let lane = Lane::watch(watch_id);
    let origins: Vec<String> = ids
        .iter()
        .map(|id| crate::map::decisive_origin(&lane, id))
        .collect();
    (
        bundle_id("bind-1", watch_id, &ids),
        origins,
        events.len() as u32,
    )
}

#[test]
fn bundle_window_keeps_all_when_at_or_under_max() {
    let events = vec![
        timeline_event(2, 20),
        timeline_event(1, 10),
        timeline_event(3, 30),
    ];
    let window = take_bundle_window(events, 50);
    assert_eq!(window.discarded, 0);
    assert_eq!(
        window
            .events
            .iter()
            .map(|e| e.created_at)
            .collect::<Vec<_>>(),
        vec![10, 20, 30]
    );
}

#[test]
fn bundle_window_keeps_newest_50_when_over_max() {
    let mut events: Vec<WatchEvent> = (1u8..=60)
        .map(|n| timeline_event(n, i64::from(n)))
        .collect();
    events.reverse();
    let window = take_bundle_window(events, 50);
    assert_eq!(window.discarded, 10);
    assert_eq!(window.events.len(), 50);
    let kept: Vec<i64> = window.events.iter().map(|e| e.created_at).collect();
    assert_eq!(kept.first().copied(), Some(11));
    assert_eq!(kept.last().copied(), Some(60));
    assert_eq!(kept, (11..=60).collect::<Vec<i64>>());
}

#[test]
fn bundle_discarded_count_is_recorded() {
    let metrics = SaidMetrics::default();
    let events: Vec<WatchEvent> = (1u8..=60)
        .map(|n| timeline_event(n, i64::from(n)))
        .collect();
    let window = take_bundle_window(events, 50);
    record_bundle_discarded(&metrics, window.discarded, window.events.len(), 50);
    assert_eq!(metrics.bundle_discarded.load(Ordering::Relaxed), 10);
    record_bundle_discarded(&metrics, 3, 50, 50);
    assert_eq!(metrics.bundle_discarded.load(Ordering::Relaxed), 13);
    record_bundle_discarded(&metrics, 0, 2, 50);
    assert_eq!(metrics.bundle_discarded.load(Ordering::Relaxed), 13);
}

#[test]
fn capped_bundle_manifest_matches_coordinator_contract() {
    let events: Vec<WatchEvent> = (1u8..=60)
        .map(|n| timeline_event(n, i64::from(n)))
        .collect();
    let window = take_bundle_window(events, 50);
    let (bundle, origins, count) = prepared_manifest(17, &window.events);
    assert_eq!(count, 50);
    assert_eq!(origins.len(), count as usize);
    let dropped = timeline_event(1, 1);
    let dropped_origin = crate::map::decisive_origin(&Lane::watch(17), &dropped.id);
    assert!(
        !origins.contains(&dropped_origin),
        "discarded origin must not enter manifest"
    );
    let kept_ids: Vec<String> = window
        .events
        .iter()
        .map(|e| crate::map::normalize_author_id(&e.id).unwrap())
        .collect();
    assert_eq!(bundle, bundle_id("bind-1", 17, &kept_ids));
    let all_ids: Vec<String> = (1u8..=60)
        .map(|n| crate::map::normalize_author_id(&hex_id(n)).unwrap())
        .collect();
    assert_ne!(
        bundle,
        bundle_id("bind-1", 17, &all_ids),
        "bundle_id must not include discarded ids"
    );
    for (i, event) in window.events.iter().enumerate() {
        let place = BundlePlace {
            bundle_id: bundle.clone(),
            index: (i as u32) + 1,
            count,
            origins: origins.clone(),
        };
        assert!(place.index >= 1 && (place.index as usize) <= origins.len());
        assert_eq!(place.origins.len(), place.count as usize);
        let mapped = map_event(
            event,
            &"11".repeat(32),
            true,
            &Lane::watch(17),
            Some(&place),
        )
        .expect("map");
        assert!(mapped
            .text
            .contains(&crate::map::bundle_members_line(&origins)));
        assert!(mapped.text.contains(&format!("\"count\":{count}")));
        assert!(!mapped.text.contains(&dropped_origin));
    }
}
