//! #628 条件 B・C: 登録簿を反復する generic な transport 検査。
//!
//! **本番と同じ源**（`register_production_descriptors`）で登録した [`TimedFireRouter`] を組み、
//! **登録簿を反復して**次を検査する。手で積んだ登録簿ではないので、本番へ transport を足せば
//! ここも自動で追随する（新しい transport は実装者がテスト行を足さなくても検査対象になる・
//! descriptor が `sample_target` を返せる形にしてあるのがその要）。
//!
//! - **条件 C（round-trip）**: どの descriptor も `build_session_id(sample) → parse → sample` が
//!   戻り、登録簿の `resolve_target` が同じ発火先へ解決する（build と parse が独立実装なので
//!   恒真にならない）。
//! - **条件 B（prefix 排他）**: どの 2 descriptor も同じ session_id を parse しない。first-match の
//!   `resolve_target` が登録順に依存して片方を黙って影に入れることを防ぐ。**起動時にも本番登録簿
//!   そのもので同じ検査が走る**（[`opencrab_actions::TimedFireRouter::self_check`] の PrefixCollision）。

use opencrab_actions::TimedFireRouter;

const AGENT_UUID: &str = "11111111-1111-4111-8111-111111111111";

/// 本番と同じ源（`register_production_descriptors`）で登録した登録簿。新 transport を本番へ
/// 足せば、この generic テストが自動で反復する（手書き registry への追記漏れが起きない）。
fn registry() -> TimedFireRouter {
    let router = TimedFireRouter::new();
    opencrab_server::register_production_descriptors(&router);
    router
}

/// 条件 C: 登録簿の各 descriptor で build ↔ parse が round-trip し、`resolve_target` が同じ
/// 発火先へ解決する。
// #654: 登録簿は feature 依存の descriptor（DiscordFire / NostrFire / WebFire・#651）だけで満ちる。
// 全 feature off では登録簿が空で「1 つ以上を round-trip」が成立しない。少なくとも 1 つの transport
// が入るときだけ意味を持つので、いずれかの feature がある構成に囲む（残る 2 test は空でも恒真）。
#[cfg(any(feature = "discord", feature = "nostr", feature = "web"))]
#[test]
fn every_descriptor_round_trips_through_the_registry() {
    let router = registry();
    let kinds: Vec<&'static str> = router.descriptor_kinds().into_iter().collect();
    assert!(!kinds.is_empty(), "descriptor が 1 つも登録されていない");

    for kind in kinds {
        let d = router
            .descriptor(kind)
            .expect("kind の descriptor が引けない");
        let sample = d.sample_target();
        assert_eq!(
            sample.kind, kind,
            "sample_target の kind が自分と違う: {kind}"
        );

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
        assert_eq!(
            resolved, sample,
            "{kind}: resolve_target の結果が sample と違う"
        );
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
                assert!(
                    parsed,
                    "{a} が自分の sample session_id を parse しない: {sid}"
                );
            } else {
                assert!(
                    !parsed,
                    "prefix 排他違反: {b} が {a} の session_id を parse した（first-match で影に入る）: {sid}"
                );
            }
        }
    }
}

/// 条件 B（起動時経路）: 本番登録簿を `self_check` に通しても PrefixCollision は出ない
/// （実在 3 transport は書式が分離している）。起動時に prefix 衝突を検出する経路自体が本番
/// descriptor に対して誤検出しないことを固定する（`self_check` は手書き registry に依存しない）。
#[test]
fn production_registry_has_no_prefix_collision_via_self_check() {
    let router = registry();
    let conn = opencrab_db::init_memory().unwrap();
    let configured = std::collections::HashSet::new();
    let env = opencrab_actions::TransportFireEnv {
        conn: &conn,
        configured_shared_kinds: &configured,
    };
    let issues = router.self_check(&env);
    assert!(
        !issues.iter().any(|i| matches!(
            i,
            opencrab_actions::TimedFireSelfCheckIssue::PrefixCollision { .. }
        )),
        "本番 transport が prefix 衝突している: {issues:?}"
    );
}
