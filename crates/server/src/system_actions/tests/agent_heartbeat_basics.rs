use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

/// own 定義に 1 件ずつ露出し、**廃止スコープ引数の痕跡がゼロ**であることを固定する
/// （#456 受け入れ基準）。`agent_id` も無い（他人を指す経路を作らない）。
#[test]
fn agent_heartbeat_tools_have_no_scope_args() {
    let defs = SystemGatewayActions::own_definitions();
    for name in ["get_my_heartbeat", "set_my_heartbeat"] {
        assert_eq!(
            defs.iter().filter(|d| d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要"
        );
        let def = defs.iter().find(|d| d.name == name).unwrap();
        let props = def
            .parameters
            .get("properties")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();
        for forbidden in ["scope", "channel_id", "guild_id", "agent_id"] {
            assert!(
                !props.contains_key(forbidden),
                "{name} に廃止引数 {forbidden} を生やしてはならない（#456）"
            );
        }
        // schema 文字列全体でも痕跡ゼロ（enum 値・説明文含め）。
        let schema = def.parameters.to_string();
        for forbidden in ["scope", "channel_id", "guild_id", "agent_id"] {
            assert!(
                !schema.contains(forbidden),
                "{name} の parameters に {forbidden} の痕跡が残っている"
            );
        }
    }
    let set = defs.iter().find(|d| d.name == "set_my_heartbeat").unwrap();
    let props = set.parameters["properties"].as_object().unwrap();
    for key in ["enabled", "interval_secs"] {
        assert!(props.contains_key(key), "missing property: {key}");
    }
}

/// #394 の教訓（道具は説明が無いと使われない）を説明文で担保する。オーナー発端は
/// エージェントが「next_run_at が無い」と実在しない名前で呼んだこと。正しい名前
/// （`next_fire_at`）と、その意味・形式（UTC RFC3339）・null になる条件・`gated` の
/// 意味が説明文に書かれていることを固定する（別名は作らない＝二重語彙を増やさない）。
#[test]
fn get_my_heartbeat_description_explains_next_fire_at_and_gating() {
    let defs = SystemGatewayActions::own_definitions();
    let desc = &defs
        .iter()
        .find(|d| d.name == "get_my_heartbeat")
        .unwrap()
        .description;
    for needle in ["next_fire_at", "RFC3339", "UTC", "null", "gated"] {
        assert!(
            desc.contains(needle),
            "get_my_heartbeat の説明文に '{needle}' が必要（#394）: {desc}"
        );
    }
    // gated=true でも next_fire_at は非 null（ゲート解除後に発火する時刻）。その 1 フィールド
    // だけ読んでも「この時刻に発火する」と誤読しないよう、意味を説明文で確定する（#394）。
    assert!(
        desc.contains("この時刻が来ても実際には発火しない"),
        "gated 時の next_fire_at の意味が説明文に無い（#394）: {desc}"
    );
    // 別名 next_run_at は作らない（二重語彙を増やさない・#456）。
    assert!(
        !desc.contains("next_run_at"),
        "next_run_at 別名を説明文に持ち込まない（#456）"
    );
}

/// **既定は無効**（#240）。設定したことが無いセッションは無効で返る。応答に廃止フィールド
/// （scope/channel_id）が無く、`next_fire_at` フィールドが存在する（#439-4）。
// #654: nostr セッションの発火経路（NostrFire descriptor）は nostr feature 時のみ登録される
// （#651）。off では fail-closed になり、検証対象の発火計算そのものが存在しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn get_my_heartbeat_defaults_to_disabled() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let r = actions
        .execute("get_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    assert!(r.success, "{:?}", r.error);
    let d = r.data.unwrap();
    assert_eq!(d["session_id"], "nostr-agent-x");
    assert_eq!(d["enabled"], false);
    assert_eq!(d["interval_secs"], 1800, "既定へフォールバック");
    assert_eq!(d["configured_interval_secs"], serde_json::Value::Null);
    assert_eq!(
        d["next_fire_at"],
        serde_json::Value::Null,
        "無効は next_fire_at=null"
    );
    assert_eq!(d["gated"], false);
    assert_eq!(d["gated_reason"], serde_json::Value::Null);
    assert_eq!(d["min_interval_secs"], 300);
    assert_eq!(d["max_interval_secs"], 86400);
    assert_eq!(d["default_interval_secs"], 1800);
    assert!(d.get("scope").is_none(), "応答に scope を残さない");
    assert!(
        d.get("channel_id").is_none(),
        "応答に channel_id を残さない"
    );
}

/// 有効化 + 間隔設定が DB に載り、`next_fire_at` が算出されて未来を指す（#439-4）。
/// nostr は G 非依存なので gated にならない。有効化で anchor=now・last_fired=NULL（§4.4）。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn set_my_heartbeat_enables_and_computes_next_fire_at() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let before = chrono::Utc::now();
    let r = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &nostr_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let d = r.data.unwrap();
    assert_eq!(d["success"], true);
    assert_eq!(d["enabled"], true);
    assert_eq!(d["interval_secs"], 600);
    assert_eq!(d["configured_interval_secs"], 600);
    assert_eq!(d["gated"], false, "nostr は G 非依存で gated にならない");
    assert_eq!(
        d["last_fired_at"],
        serde_json::Value::Null,
        "有効化で last_fired はリセット（§4.4）"
    );
    let anchor = chrono::DateTime::parse_from_rfc3339(d["anchor_at"].as_str().unwrap()).unwrap();
    assert!(
        anchor >= before - chrono::Duration::seconds(2),
        "有効化で anchor=now"
    );
    let nf = chrono::DateTime::parse_from_rfc3339(
        d["next_fire_at"].as_str().expect("next_fire_at 必須"),
    )
    .unwrap();
    assert!(nf > chrono::Utc::now(), "next_fire は未来（now+interval）");
    // DB へ反映（get で読み直し）。
    let g = actions
        .execute("get_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    assert_eq!(g.data.unwrap()["enabled"], true);
}

/// 下限より短い間隔は**拒否**し（丸めない）、DB に一切書かない。
#[tokio::test]
async fn set_my_heartbeat_rejects_interval_below_floor_without_writing() {
    let state = heartbeat_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 1}),
            &nostr_ctx(),
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("短すぎ"));
    let conn = state.db.lock().unwrap();
    assert!(
        opencrab_db::queries::get_session_heartbeat_config(&conn, "agent-x", "nostr-agent-x")
            .unwrap()
            .is_none(),
        "拒否時は行を作らない"
    );
}

/// enabled も interval も無ければエラー。
#[tokio::test]
async fn set_my_heartbeat_bad_args() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    let r = actions
        .execute("set_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.unwrap(),
        "enabled か interval_secs のどちらかが必要です"
    );
}

/// 他エージェントを指す引数（agent_id 等）は黙殺せず明示エラー。
#[tokio::test]
async fn set_my_heartbeat_cannot_target_another_agent() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    for key in ["agent_id", "target_agent_id", "agent"] {
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({key: "victim", "enabled": true}),
                &nostr_ctx(),
            )
            .await;
        assert!(!r.success, "{key} を無視してはいけない");
        assert!(r.error.unwrap().contains(key));
    }
}

/// 廃止したスコープ引数（scope/channel_id/guild_id）は黙殺せず**廃止を明示**して誘導する。
#[tokio::test]
async fn heartbeat_tools_reject_removed_scope_args() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    for key in ["scope", "channel_id", "guild_id"] {
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({key: "channel", "enabled": true}),
                &nostr_ctx(),
            )
            .await;
        assert!(!r.success, "{key} は廃止・黙殺しない（#456）");
        assert!(r.error.unwrap().contains("廃止"));
    }
    // get も同様。
    let g = actions
        .execute(
            "get_my_heartbeat",
            &json!({"scope": "channel"}),
            &nostr_ctx(),
        )
        .await;
    assert!(!g.success);
    assert!(g.error.unwrap().contains("廃止"));
}

/// 発火経路の無いセッション（session_id なし / web-）は fail-closed（設計 §13.1）。
/// 「設定できたのに永遠に発火しない行」を作らせない。**エラーには理由だけでなく remedy
/// （どこで実行すればよいか）を書く**（#456 の発端は混乱・M-b）。詰まらせて終わらない。
// #654: この test は remedy 文言が Discord と Nostr の両方を含むこと（fire_target_hint が両
// descriptor を畳む）を検証する。両 descriptor は各 feature 時のみ登録される（#651）ので、両方の
// feature が揃うときだけ意味を持つ。off では hint が空になり検証が成立しないので同じ cfg で囲む。
#[cfg(all(feature = "discord", feature = "nostr"))]
#[tokio::test]
async fn heartbeat_tools_fail_closed_without_fireable_session() {
    let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
    // remedy 相当の文言（次に何をすればよいかが 1 読で分かる）が含まれること。
    let has_remedy = |msg: &str| {
        msg.contains("Discord") && msg.contains("Nostr") && msg.contains("実行してください")
    };
    // (a) セッション文脈なし。
    let mut none_ctx = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    none_ctx.session_id = None;
    let r = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &none_ctx)
        .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("セッション文脈"), "理由: {e}");
    assert!(has_remedy(&e), "remedy が無い（詰まらせる）: {e}");
    // (b) 発火経路の無い種別（web-）。
    let mut web = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    web.session_id = Some("web-agent-x".to_string());
    let r = actions
        .execute("set_my_heartbeat", &json!({"enabled": true}), &web)
        .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("発火経路"), "理由: {e}");
    assert!(has_remedy(&e), "remedy が無い（詰まらせる）: {e}");
    // get も fail-closed かつ remedy 付き。
    let r = actions.execute("get_my_heartbeat", &json!({}), &web).await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(e.contains("発火経路"), "理由: {e}");
    assert!(has_remedy(&e), "remedy が無い（詰まらせる）: {e}");
}

/// `discord-` セッションは G=false のとき「enabled なのに発火しない」理由を本人へ見せる
/// （#394 / #4）。**whitelist は理由に含めない**（現行発火経路にゲートとして無い・§5 N3）。
// #654: discord セッションの発火経路（DiscordFire descriptor）は discord feature 時のみ登録される
// （#651）。off では discord_ctx が fail-closed になり G ゲート理由を検証できないので同じ cfg で囲む。
#[cfg(feature = "discord")]
#[tokio::test]
async fn get_my_heartbeat_shows_discord_gated_when_global_g_is_false() {
    let state = heartbeat_state_with_g(false);
    let actions = SystemGatewayActions::new(state, None, None, None);
    let s = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &discord_ctx(),
        )
        .await;
    assert!(s.success, "{:?}", s.error);
    let d = s.data.unwrap();
    assert_eq!(d["enabled"], true);
    assert_eq!(d["gated"], true, "G=false の discord は gated");
    let reason = d["gated_reason"].as_str().unwrap();
    assert!(reason.contains("グローバル"), "理由に G を示す: {reason}");
    assert!(
        !reason.contains("whitelist"),
        "whitelist を理由にしない（嘘・§5 N3）"
    );
}

/// G=true なら `discord-` セッションは gated でない。
// #654: discord セッションの発火経路は discord feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "discord")]
#[tokio::test]
async fn discord_not_gated_when_global_g_is_true() {
    let state = heartbeat_state_with_g(true);
    let actions = SystemGatewayActions::new(state, None, None, None);
    let s = actions
        .execute(
            "set_my_heartbeat",
            &json!({"enabled": true, "interval_secs": 600}),
            &discord_ctx(),
        )
        .await;
    let d = s.data.unwrap();
    assert_eq!(d["gated"], false, "G=true の discord は gated でない");
    assert_eq!(d["gated_reason"], serde_json::Value::Null);
}

/// 壊れた間隔（0 以下）で enabled の行は、実効 null・next_fire_at null・gated（理由=間隔）。
/// set 経路は <=0 を拒否するので DB へ直接書いて経路を作る（保険ゲートの可視化）。
// #654: nostr セッションの発火経路は nostr feature 時のみ登録される（#651）。off は fail-closed。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn get_my_heartbeat_gates_on_broken_interval() {
    let state = heartbeat_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_session_heartbeat_config(
            &conn,
            &opencrab_db::queries::SessionHeartbeatConfigRow {
                agent_id: "agent-x".into(),
                session_id: "nostr-agent-x".into(),
                enabled: true,
                interval_secs: Some(0),
                anchor_at: Some(chrono::Utc::now().to_rfc3339()),
                last_fired_at: None,
            },
        )
        .unwrap();
    }
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("get_my_heartbeat", &json!({}), &nostr_ctx())
        .await;
    let d = r.data.unwrap();
    assert_eq!(d["enabled"], true);
    assert_eq!(
        d["interval_secs"],
        serde_json::Value::Null,
        "壊れた間隔は実効 null"
    );
    assert_eq!(d["next_fire_at"], serde_json::Value::Null);
    assert_eq!(d["gated"], true);
    assert!(d["gated_reason"].as_str().unwrap().contains("間隔"));
}
