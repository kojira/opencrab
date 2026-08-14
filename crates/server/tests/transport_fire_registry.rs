//! #628 条件 B・C: 登録簿を反復する generic な transport 検査。
//!
//! 本番（main.rs）と同じ descriptor を登録した [`TimedFireRouter`] を組み、**登録簿を反復して**
//! 次を検査する。新しい transport を登録すれば実装者がテスト行を足さなくても自動で検査対象に
//! なる（descriptor が `sample_target` を返せる形にしてあるのがその要）。
//!
//! - **条件 C（round-trip）**: どの descriptor も `build_session_id(sample) → parse → sample` が
//!   戻り、登録簿の `resolve_target` が同じ発火先へ解決する（build と parse が独立実装なので
//!   恒真にならない）。
//! - **条件 B（prefix 排他）**: どの 2 descriptor も同じ session_id を parse しない。first-match の
//!   `resolve_target` が登録順に依存して片方を黙って影に入れることを防ぐ。

use std::sync::Arc;

use opencrab_actions::TimedFireRouter;

const AGENT_UUID: &str = "6b79ac3a-7f17-4618-a827-5bda992a3698";

/// 本番と同じ transport descriptor を登録した登録簿（新 transport はここへ足すと下の
/// generic テストが自動で拾う）。
fn registry() -> TimedFireRouter {
    let router = TimedFireRouter::new();
    #[cfg(feature = "discord")]
    router.register_descriptor(Arc::new(opencrab_discord::DiscordFire));
    router.register_descriptor(Arc::new(opencrab_nostr::NostrFire));
    router
}

/// 条件 C: 登録簿の各 descriptor で build ↔ parse が round-trip し、`resolve_target` が同じ
/// 発火先へ解決する。
#[test]
fn every_descriptor_round_trips_through_the_registry() {
    let router = registry();
    let kinds: Vec<&'static str> = router.descriptor_kinds().into_iter().collect();
    assert!(!kinds.is_empty(), "descriptor が 1 つも登録されていない");

    for kind in kinds {
        let d = router.descriptor(kind).expect("kind の descriptor が引けない");
        let sample = d.sample_target();
        assert_eq!(sample.kind, kind, "sample_target の kind が自分と違う: {kind}");

        // build → parse → sample（両方向・独立実装）。
        let sid = d.build_session_id(&sample, AGENT_UUID);
        assert_eq!(
            d.parse(&sid, AGENT_UUID).as_ref(),
            Some(&sample),
            "{kind}: build した session_id を自分で parse し戻せない: {sid}"
        );

        // 登録簿（first-match）も同じ発火先へ解決する。
        let resolved = router
            .resolve_target(&sid, AGENT_UUID)
            .unwrap_or_else(|| panic!("{kind}: resolve_target が解決できない: {sid}"));
        assert_eq!(resolved, sample, "{kind}: resolve_target の結果が sample と違う");
    }
}

/// 条件 B: どの 2 descriptor も同じ session_id を parse しない（全ペア）。
///
/// これが破れると、登録順で片方が黙って影に入る（first-match が別 transport を横取りする）。
#[test]
fn no_two_descriptors_parse_the_same_session_id() {
    let router = registry();
    let kinds: Vec<&'static str> = router.descriptor_kinds().into_iter().collect();

    for a in &kinds {
        let da = router.descriptor(a).unwrap();
        let sid = da.build_session_id(&da.sample_target(), AGENT_UUID);
        for b in &kinds {
            let db = router.descriptor(b).unwrap();
            let parsed = db.parse(&sid, AGENT_UUID).is_some();
            if a == b {
                assert!(parsed, "{a} が自分の sample session_id を parse しない: {sid}");
            } else {
                assert!(
                    !parsed,
                    "prefix 排他違反: {b} が {a} の session_id を parse した（first-match で影に入る）: {sid}"
                );
            }
        }
    }
}
