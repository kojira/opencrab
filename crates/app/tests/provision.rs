//! 設定から場を起こす配線（タスク#1）の**決定的**な検証（ネットワーク無し・CI で走る）。
//!
//! 実リレーでの通し（tests/nostr_e2e.rs・#[ignore]）とは別に、ここでは「設定 → 場 → 発火方針 → ゲートへの
//! 結び → 名寄せの身元」が、ゲート名を直書きせずデータから組み上がることだけを固定する。壊れた設定を
//! 既定へ倒さない（§15）ことも確かめる。
//!
//! これが是正した配線漏れの核: バイナリは web の場を 1 つ用意するだけで、Nostr の住所を場に結ぶ経路も、
//! まとめ窓を持つ場を作る経路も無かった。`parse_places_config` + `provision_place` がその経路。

use opencrab_app::{parse_places_config, Host};
use opencrab_port::GateName;
use opencrab_social_runtime::Policy;
use opencrab_store::Store;

const CONFIG: &str = r#"{
  "places": [
    {"address": "room:main", "gate": "web", "name": "web-agent", "persona": "あなたは web の窓口です。",
     "policy": {"immediate": ["direct"], "immediate_from": "anyone",
                "batch_window_ms": null, "unconditional_interval_ms": null}},
    {"address": "filter:kind=1&author=npub1abc", "gate": "nostr", "name": "エージェントA", "persona": "あなたはエージェントAです。",
     "policy": {"immediate": ["mentions_me", "replies_to_me"], "immediate_from": "anyone",
                "batch_window_ms": 8000, "unconditional_interval_ms": null},
     "identities": [{"gate": "nostr", "external": "npub1abc"}]}
  ]
}"#;

#[test]
fn config_raises_named_gate_places_with_policy_and_identity() {
    let specs = parse_places_config(CONFIG).expect("parse config");
    assert_eq!(specs.len(), 2);

    let store = Store::new_in_memory().unwrap();
    let host = Host::boot_with_engine(store, std::sync::Arc::new(opencrab_app::EchoEngine));

    let mut nostr_place = None;
    let mut nostr_agent = None;
    for spec in &specs {
        let (place, agent) = host.provision_place(spec);
        if spec.gate == "nostr" {
            nostr_place = Some(place);
            nostr_agent = Some(agent);
        }
    }
    let nostr_place = nostr_place.unwrap();
    let nostr_agent = nostr_agent.unwrap();
    let store = host.sys.store();

    // 住所は**設定で与えたゲート名**に結ばれる（web でも nostr でも同じ 1 本が結ぶ・直書きしない）。
    let nostr_chans = store.channels_for_gate(&GateName::new("nostr")).unwrap();
    assert!(
        nostr_chans
            .iter()
            .any(|(_p, a)| a == "filter:kind=1&author=npub1abc"),
        "nostr の住所が nostr ゲートに結ばれる: {nostr_chans:?}"
    );
    let web_chans = store.channels_for_gate(&GateName::new("web")).unwrap();
    assert!(
        web_chans.iter().any(|(_p, a)| a == "room:main"),
        "web の住所が web ゲートに結ばれる"
    );

    // 発火方針: メンション・返信だけ即応（direct は入れない）・窓でまとめる・既定は用意した主体。
    let row = store.get_place(nostr_place).unwrap().unwrap();
    let pol = Policy::from_json(&row.policy_json).unwrap();
    assert_eq!(pol.batch_window_ms, Some(8000), "まとめ窓が設定から入る");
    assert_eq!(
        pol.default_subject,
        Some(nostr_agent),
        "宛先が無いときに返すのは用意した主体（default_subject は主体で埋まる）"
    );
    let pj = pol.to_json();
    assert!(
        pj.contains("mentions_me") && pj.contains("replies_to_me"),
        "即応はメンションと返信: {pj}"
    );
    assert!(
        !pj.contains("direct"),
        "タイムラインの平の発話（direct）は即応にしない＝溜める: {pj}"
    );

    // 名寄せの身元: エージェントは自分の npub から解決できる（これが無いと「自分宛の言及」が解けない）。
    let resolved = store
        .resolve_subject(&GateName::new("nostr"), "npub1abc")
        .unwrap();
    assert_eq!(
        resolved,
        Some(nostr_agent),
        "エージェントの Nostr 上の身元が名寄せに載る"
    );
}

#[test]
fn broken_config_is_not_bent_to_a_default() {
    // immediate_from の未知値を緩い方へ倒さない（§15・Policy::from_json の姿勢を設定読みでも保つ）。
    let bad = r#"{"places":[{"address":"room:x","gate":"web","name":"a","persona":"a",
        "policy":{"immediate":[],"immediate_from":"whoever"}}]}"#;
    assert!(
        parse_places_config(bad).is_err(),
        "未知の immediate_from は Err"
    );

    // 未知の性質名も Err（近いものへ寄せない）。
    let bad2 = r#"{"places":[{"address":"room:x","gate":"web","name":"a","persona":"a",
        "policy":{"immediate":["shout"],"immediate_from":"anyone"}}]}"#;
    assert!(parse_places_config(bad2).is_err(), "未知の性質名は Err");

    // 必須欄の欠落は Err（既定で埋めない）。
    let bad3 = r#"{"places":[{"gate":"web","persona":"a",
        "policy":{"immediate":[],"immediate_from":"anyone"}}]}"#;
    assert!(parse_places_config(bad3).is_err(), "address 欠落は Err");

    // `places` が無い設定も Err。
    assert!(parse_places_config("{}").is_err(), "places 欠落は Err");
}
