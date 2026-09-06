use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

/// 明示の無効化は anchor/last_fired を触らない（位相保存・再有効化まで保つ）。next_fire_at は null。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_disable_keeps_phase() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let e = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let anchor1 = e.data.unwrap()["anchor_at"].as_str().unwrap().to_string();
    let d = actions
        .execute("set_my_heartbeat", &json!({"enabled": false}), &nostr_ctx())
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["enabled"], false);
    assert_eq!(
        data["anchor_at"].as_str().unwrap(),
        anchor1,
        "無効化で anchor を触らない（§4.4）"
    );
    assert_eq!(data["next_fire_at"], serde_json::Value::Null);
}

/// #605: 間隔変更は anchor を now へ張り直さない（起点を据え置く）。以前は毎回 now へ
/// リセットしていたため、調整のたびに次回発火が先送りされて発火しなかった。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_interval_change_preserves_anchor() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let e = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 3600}),
            &nostr_ctx(),
        )
        .await;
    let anchor1 = e.data.unwrap()["anchor_at"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["enabled"], true, "enabled は保持");
    assert_eq!(data["interval_secs"], 600);
    assert_eq!(
        data["anchor_at"].as_str().unwrap(),
        anchor1,
        "間隔変更で anchor を据え置く（#605: now へ張り直さない）"
    );
}

/// #605 の本丸: 設定変更で `last_fired_at`（発火した事実）を消さない。消すと next_fire が
/// anchor 基準へ戻り、調整のたびに位相が先送りされて発火しなくなる。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_preserves_last_fired_across_config_change() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 3600}),
            &nostr_ctx(),
        )
        .await;
    // 「実際に発火した」事実を刻む（発火経路だけが行う操作を模す）。
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    // enabled は変えず interval だけ 600 へ。
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["interval_secs"], 600);
    assert_eq!(
        data["last_fired_at"].as_str().unwrap(),
        fired_at,
        "設定変更で last_fired が消えた（#605 の退行）"
    );
    // next_fire = last_fired + interval（now 基準へ張り直さない）。
    let got = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap()).unwrap();
    let exp =
        chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap() + chrono::Duration::seconds(600);
    assert_eq!(
        got, exp,
        "next_fire は last_fired+interval であるべき（now 基準ではない）"
    );
}

/// #605 対称ケース: 間隔の**延長**でも last_fired を保ち、next_fire = last_fired+（延ばした）interval。
/// 短縮ケース（preserves_last_fired_across_config_change）と経路は同一だが、対称性のため延長方向も明示する。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_preserves_last_fired_when_interval_extended() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    // 「実際に発火した」事実を刻む（発火経路だけが行う操作を模す）。
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    // enabled は変えず interval を 600 → 7200 へ**延長**。
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 7200}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["interval_secs"], 7200);
    assert_eq!(
        data["last_fired_at"].as_str().unwrap(),
        fired_at,
        "間隔延長で last_fired が消えた（#605 の退行）"
    );
    // next_fire = last_fired + interval（延ばした 7200 を使う。now 基準へ張り直さない）。
    let got = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap()).unwrap();
    let exp =
        chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap() + chrono::Duration::seconds(7200);
    assert_eq!(
        got, exp,
        "next_fire は last_fired+（延ばした）interval であるべき（now 基準ではない）"
    );
    // last_fired が -120 秒でも 7200 秒後は十分未来＝延長で発火が先送りされる。
    assert!(
        got > chrono::Utc::now(),
        "延長後の next_fire は未来（+7200）であるべき: {got}"
    );
}

/// #605: 発火済みセッションの**再有効化**でも last_fired を保つ（→ next_fire = last_fired+interval。
/// 過ぎていれば即発火する）。以前は再有効化で last_fired=NULL・anchor=now になり先送りされた。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_reenable_after_fire_preserves_last_fired() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    let _ = actions
        .execute("set_my_heartbeat", &json!({"enabled": false}), &nostr_ctx())
        .await;
    let d = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &nostr_ctx())
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["enabled"], true);
    assert_eq!(
        data["last_fired_at"].as_str().unwrap(),
        fired_at,
        "再有効化で last_fired を消さない（#605）"
    );
    let got = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap()).unwrap();
    let exp =
        chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap() + chrono::Duration::seconds(600);
    assert_eq!(
        got, exp,
        "next_fire = last_fired+interval（now+interval へ逃がさない）"
    );
}

/// #605: 初回有効化は従来どおり anchor=now を打ち、next_fire = now+interval（enable 直後の
/// 即発火は避ける）。last_fired はまだ無い。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_first_enable_sets_anchor_to_now() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let before = chrono::Utc::now();
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(
        data["last_fired_at"],
        serde_json::Value::Null,
        "初回は未発火"
    );
    let anchor = chrono::DateTime::parse_from_rfc3339(data["anchor_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        anchor >= before - chrono::Duration::seconds(5)
            && anchor <= chrono::Utc::now() + chrono::Duration::seconds(5),
        "初回有効化は anchor を now 付近に打つ: {anchor}"
    );
    let next = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        next > chrono::Utc::now(),
        "初回有効化の next_fire は未来（now+interval・即発火しない）: {next}"
    );
}

/// #605 の目玉を直接 assert: `last_fired + interval < now` なら next_fire は**過去**（＝即発火）。
/// 既存テストは last_fired が -30/-120 秒で next_fire が常に未来だったため、この核心を守っていなかった。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_next_fire_is_in_the_past_when_overdue() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    // 前回発火を interval より前（2000 秒前）に置く → last_fired + 600 は過去。
    let fired_at = (chrono::Utc::now() - chrono::Duration::seconds(2000)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_last_fired(&conn, "agent-x", "nostr-agent-x", &fired_at)
            .unwrap();
    }
    // 設定変更（再有効化）。last_fired は保持され、next_fire は過去のまま＝即発火扱い。
    let d = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &nostr_ctx())
        .await;
    let data = d.data.unwrap();
    assert_eq!(data["last_fired_at"].as_str().unwrap(), fired_at);
    let next = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        next < chrono::Utc::now(),
        "next_fire は過去であるべき（即発火）: {next}"
    );
    let exp = (chrono::DateTime::parse_from_rfc3339(&fired_at).unwrap()
        + chrono::Duration::seconds(600))
    .with_timezone(&chrono::Utc);
    assert_eq!(next, exp, "next_fire = last_fired + interval");
}

/// #605 doc の 2 ケース目: **未発火 + 古い anchor + 間隔短縮**でも next_fire は過去＝即発火。
/// anchor を据え置く（now へ張り直さない）ので `anchor+新interval` が過ぎれば直ちに発火する。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_never_fired_old_anchor_shorten_fires_immediately() {
    let state = heartbeat_state();
    // 未発火・古い anchor（10000 秒前）・enabled・長い間隔の行を直接用意する。
    let old_anchor = (chrono::Utc::now() - chrono::Duration::seconds(10000)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_session_heartbeat_config(
            &conn,
            &opencrab_db::queries::SessionHeartbeatConfigRow {
                agent_id: "agent-x".into(),
                session_id: "nostr-agent-x".into(),
                enabled: true,
                interval_secs: Some(3600),
                anchor_at: Some(old_anchor.clone()),
                last_fired_at: None,
            },
        )
        .unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    // 間隔を 600 へ短縮（enabled 引数なし）。anchor は据え置き（古いまま）。
    let d = actions
        .execute(
            "set_my_heartbeat",
            &json!({"interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let data = d.data.unwrap();
    assert_eq!(
        data["last_fired_at"],
        serde_json::Value::Null,
        "未発火のまま"
    );
    assert_eq!(
        data["anchor_at"].as_str().unwrap(),
        old_anchor,
        "古い anchor は据え置き（now へ張り直さない）"
    );
    let next = chrono::DateTime::parse_from_rfc3339(data["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        next < chrono::Utc::now(),
        "anchor+interval が過去 → 即発火: {next}"
    );
    let exp = (chrono::DateTime::parse_from_rfc3339(&old_anchor).unwrap()
        + chrono::Duration::seconds(600))
    .with_timezone(&chrono::Utc);
    assert_eq!(next, exp, "next_fire = anchor + interval");
}

/// #437: set 後に中央スケジューラを起こす（即時反映）。notify の permit を消費できる。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_wakes_scheduler() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let _ = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    let woke = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        state.scheduler_wake.notified(),
    )
    .await;
    assert!(
        woke.is_ok(),
        "set_my_heartbeat は #437 で scheduler_wake を鳴らす"
    );
}

/// 未信頼の素の Agent からは get/set とも拒否（多層防御）。
#[tokio::test]
async fn agent_heartbeat_tools_reject_untrusted_agent() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let mut agent = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
    agent.session_id = Some("nostr-agent-x".to_string());
    for name in ["get_my_heartbeat", "set_my_heartbeat"] {
        let r = actions
            .execute(name, &json!({"enabled": true}), &agent)
            .await;
        assert!(!r.success, "{name} は素の Agent を拒否");
        assert!(r.error.unwrap().contains("信頼済み"));
    }
}

/// Owner は許可（自分の設定を自分で触るのが目的）。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_allows_owner() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let mut owner = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");
    owner.session_id = Some("nostr-agent-x".to_string());
    let r = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &owner)
        .await;
    assert!(r.success, "{:?}", r.error);
}
