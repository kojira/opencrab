// これらの import はどのテストも discord / nostr のマネージャを組むために使う。
// 両 feature を外した構成（例 `--no-default-features` / web のみ）ではこのモジュールの
// テストが 1 つも残らないため、import も条件付きにして未使用警告を出さない。
#[cfg(any(feature = "discord", feature = "nostr"))]
use super::*;
#[cfg(any(feature = "discord", feature = "nostr"))]
use opencrab_actions::gateway_kinds;

/// state を clone しても登録簿は**同じ 1 つ**を指す。
///
/// これが成り立たないと「共有ゲートウェイへ渡した clone からは専用ゲートウェイが
/// 見えない」ことになり、内部可変にして後から登録する意味が無くなる（#40 の
/// 二重処理防止が壊れる）。
#[cfg(feature = "nostr")]
#[test]
fn registry_is_shared_across_state_clones() {
    let state = test_app_state();
    let clone = state.clone();
    assert!(Arc::ptr_eq(&state.gateways, &clone.gateways));

    let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(
        state.clone(),
        state.timed_fire_router.clone(),
    ));
    state.gateways.register(nostr);
    assert_eq!(clone.gateways.kinds(), vec![gateway_kinds::NOSTR]);
}

/// **Discord を落とした構成**では位置 1 の走査ごと消え、残る 1 回が Nostr を復元する。
#[cfg(all(not(feature = "discord"), feature = "nostr"))]
#[tokio::test]
async fn startup_sweep_restores_nostr_without_discord() {
    let state = test_app_state();
    let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(
        state.clone(),
        state.timed_fire_router.clone(),
    ));
    state.gateways.register(nostr);

    assert_eq!(
        state.gateways.restore_pending().await,
        vec![gateway_kinds::NOSTR]
    );
    assert!(state.gateways.restore_pending().await.is_empty());
}

/// 生存確認は「稼働していない / 未登録」のどちらでも false に倒れる。
///
/// これはルーティング判定（専用ゲートウェイに任せるか、共有側が続けるか）なので、
/// 未登録で true に倒すと二重処理、panic させると停止する。
#[cfg(feature = "nostr")]
#[test]
fn is_running_falls_back_to_false() {
    let state = test_app_state();
    let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(
        state.clone(),
        state.timed_fire_router.clone(),
    ));
    state.gateways.register(nostr);

    assert!(
        !state.gateways.is_running(gateway_kinds::NOSTR, "crab"),
        "起動していないエージェントは false"
    );
    assert!(
        !state.gateways.is_running(gateway_kinds::DISCORD, "crab"),
        "未登録の種別も false（共有側が処理を続ける）"
    );
    assert!(
        !state.gateways.is_running("mcp", "crab"),
        "MCP は登録簿に入れない（受信を持たない）"
    );
}

/// トレイト経由で起動を呼べる。設定行が無ければ `Err`（panic しない）。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn start_through_trait_errors_without_db_config() {
    let state = test_app_state();
    let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(
        state.clone(),
        state.timed_fire_router.clone(),
    ));
    state.gateways.register(nostr);

    let gw = state.gateways.get(gateway_kinds::NOSTR).unwrap();
    assert!(
        gw.start("no-such-agent").await.is_err(),
        "設定を DB から読む契約なので、行が無ければ Err"
    );
    // 停止と全停止は稼働ゼロでも安全に呼べる。
    gw.stop("no-such-agent").await;
    gw.shutdown_all().await;
}

/// **鍵が未設定の Nostr 設定では起動しない。**
///
/// 移設前は `PUT /api/agents/{id}/nostr` が「鍵が無ければ 400」を呼び出しの手前で
/// 返していた（`POST /nostr/start` にはその判定が無く、素通りしていた）。判定を
/// `start_agent_gateway` の単一チョークポイントへ置き直したので、どの呼び出し口
/// からでも同じように弾かれる。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn nostr_start_declines_without_secret_key() {
    let state = test_app_state();
    let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(
        state.clone(),
        state.timed_fire_router.clone(),
    ));
    state.gateways.register(nostr);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: "agent-191-pr3".to_string(),
                secret_key: "  ".to_string(),
                relays_json: "[]".to_string(),
                filter_json: r#"{"authors":["npub1abc"],"keywords":[],"kinds":[1]}"#.to_string(),
                enabled: true,
            },
        )
        .unwrap();
    }

    let gw = state.gateways.get(gateway_kinds::NOSTR).unwrap();
    let err = gw.start("agent-191-pr3").await.unwrap_err();
    assert!(
        opencrab_actions::is_start_declined(&err),
        "鍵が無いのに起動を試みている: {err}"
    );
    assert!(!state
        .gateways
        .is_running(gateway_kinds::NOSTR, "agent-191-pr3"));
}

/// **Nostr の `start` は DB の `enabled` を見ない。**
///
/// ハンドラ側の方針が「起動が成功してから `enabled=true`」なので、`PUT /nostr` は
/// **わざと `enabled=false` の行を書いてから** `start` を呼ぶ。ここに Discord と同じ
/// 有効フラグのガードを足すと、その正しい経路が毎回自分のガードに弾かれて Nostr が
/// 二度と起動しなくなる（無効化ではなく**機能停止**）。
///
/// `enabled=false` の行で `start` を呼び、返ってくるのが**秘密鍵の拒否**であることを
/// 見る。有効フラグのガードが先に弾いていればこの文言にはならないので、「`enabled=false`
/// を素通りして資格情報の検査まで到達している」ことが分かる。
///
/// [#271/#278] 以前はここで「フィルタが無制限（author も keyword も無い）」の拒否文言を
/// 見ていた。新 nostaro では `watch` が mention-only 既定で自分宛だけを購読するため
/// **空フィルタは洪水ではなく最も狭い購読**で、そのガード自体が無くなった。テストの意図
/// （`enabled` を見ずに検査へ到達する）はそのままに、到達を確かめる対象を今も残っている
/// 資格情報ガードへ移した。鍵の拒否は設定ファイルを書き出す**手前**なので、実プロセスも
/// ファイルシステムも触らないという性質も変わらない。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn nostr_start_does_not_look_at_the_enabled_flag() {
    let state = test_app_state();
    let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(
        state.clone(),
        state.timed_fire_router.clone(),
    ));
    state.gateways.register(nostr);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(
            &conn,
            &opencrab_db::queries::AgentNostrConfigRow {
                agent_id: "agent-191-pr3".to_string(),
                // 空白だけの nsec = 資格情報ガードに弾かれる（起動は試みられる）。
                secret_key: "  ".to_string(),
                relays_json: "[]".to_string(),
                filter_json: r#"{"authors":[],"keywords":[],"kinds":[1]}"#.to_string(),
                // `PUT /nostr` が start を呼ぶ瞬間の状態そのもの。
                enabled: false,
            },
        )
        .unwrap();
    }

    let gw = state.gateways.get(gateway_kinds::NOSTR).unwrap();
    let err = gw.start("agent-191-pr3").await.unwrap_err();
    assert!(
        err.to_string().contains("秘密鍵"),
        "enabled=false の行が資格情報の検査より手前で弾かれている\
         （enabled を見るガードを足すと PUT /nostr が通らなくなる）: {err}"
    );
    assert!(
        !state
            .gateways
            .is_running(gateway_kinds::NOSTR, "agent-191-pr3"),
        "弾かれたのに稼働してはいけない"
    );
}
