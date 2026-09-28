use super::*;
use serde_json::json;

fn ctx(session_id: &str) -> GatewayCallContext {
    let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
    c.session_id = Some(session_id.to_string());
    c
}

/// 別エージェント（agent-y）の文脈。他人の id を渡す攻撃の再現に使う。
// #654: 使うのは nostr/web セッションを立てる test だけ。発火経路 descriptor は各 feature 時
// のみ登録される（#651）ので、その cfg 下でだけ使われる（bare/discord では未使用＝不要）。
#[cfg(any())]
fn ctx_for(agent_id: &str, session_id: &str) -> GatewayCallContext {
    let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, agent_id);
    c.session_id = Some(session_id.to_string());
    c
}

/// set_my_schedule は **ctx.session_id** に対して作成する（スコープ引数なし）。
// #654: nostr セッションの発火経路（NostrFire descriptor）は nostr feature 時のみ登録される
// （#651）。off では作成が fail-closed になり検証対象の挙動が存在しないので同じ cfg で囲む。
#[cfg(any())]
#[tokio::test]
async fn set_creates_on_current_session_and_get_lists_it() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let res = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "巡回してまとめを書く"}),
        &c,
    );
    assert!(res.success, "作成成功: {:?}", res.error);
    let data = res.data.unwrap();
    assert_eq!(data["session_id"], "nostr-agent-x");
    assert!(data["id"].as_i64().unwrap() > 0);
    assert_eq!(data["enabled"], true, "enabled 省略時 true");
    // next_fire_at が照会時算出される（@every 3h・anchor=now → 未来）。
    assert!(data["next_fire_at"].is_string(), "next_fire_at を返す");

    // get_my_schedules は同一セッションのものを列挙し next_fire_at を含む。
    let got = get_my_schedules(&state, &json!({}), &c);
    assert!(got.success);
    let gd = got.data.unwrap();
    assert_eq!(gd["count"], 1);
    assert!(gd["schedules"][0]["next_fire_at"].is_string());
}

/// 発火経路の無いセッション（`agent-msg-` 等・登録済み descriptor がどれも名乗らない）は
/// fail-closed + **remedy** で拒否する。
#[tokio::test]
async fn set_rejects_non_firing_session_with_remedy() {
    let state = crate::test_app_state();
    let res = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "x"}),
        &ctx("agent-msg-agent-x"),
    );
    assert!(!res.success);
    let e = res.error.unwrap();
    assert!(e.contains("発火経路"), "理由: {e}");
    assert!(e.contains("実行してください"), "remedy: {e}");
}

/// cron 式が不正ならその場でエラー（remedy 付き）。
// #654: nostr セッションで cron 検証まで到達するには NostrFire（nostr feature）が要る（#651）。
// off では発火経路解決が先に fail-closed になり cron 検証へ届かないので同じ cfg で囲む。
#[cfg(any())]
#[tokio::test]
async fn set_rejects_invalid_cron_in_the_same_turn() {
    let state = crate::test_app_state();
    let res = set_my_schedule(
        &state,
        &json!({"cron_expr": "totally not cron", "message": "x"}),
        &ctx("nostr-agent-x"),
    );
    assert!(!res.success);
    let e = res.error.unwrap();
    assert!(
        e.contains("cron") || e.contains("@every") || e.contains("不正"),
        "cron 不正 remedy: {e}"
    );
}

/// スコープ引数（session_id 等）は明示拒否（#456 の語彙統一）。
#[tokio::test]
async fn set_rejects_scope_style_args() {
    let state = crate::test_app_state();
    let res = set_my_schedule(
        &state,
        &json!({"session_id": "nostr-other", "cron_expr": "@every 3h", "message": "x"}),
        &ctx("nostr-agent-x"),
    );
    assert!(!res.success);
    assert!(res.error.unwrap().contains("session_id"));
}

/// 必須引数（cron_expr / message）欠落は remedy 付きエラー。
// #654: nostr セッションで必須引数検証まで到達するには NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn set_requires_cron_and_message() {
    let state = crate::test_app_state();
    let res = set_my_schedule(&state, &json!({"message": "x"}), &ctx("nostr-agent-x"));
    assert!(!res.success);
    assert!(res.error.unwrap().contains("cron_expr"));
}

// ---- #477: update / delete ----

/// 自分のスケジュールを作って id を取り出すヘルパ。
// #654: nostr セッションで作成する test 専用のヘルパ。NostrFire descriptor は nostr feature
// 時のみ登録される（#651）ので同じ cfg で囲む。
#[cfg(any())]
fn create_one(state: &AppState, c: &GatewayCallContext) -> i64 {
    let res = set_my_schedule(
        state,
        &json!({"cron_expr": "@every 3h", "message": "巡回してまとめを書く"}),
        c,
    );
    assert!(res.success, "作成成功: {:?}", res.error);
    res.data.unwrap()["id"].as_i64().unwrap()
}

/// update に enabled=false を渡すと「止まる」が**行は残る**（履歴が追える）。
// #654: nostr セッションで作成→更新する。NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn update_disable_stops_but_keeps_row() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let id = create_one(&state, &c);

    let res = update_my_schedule(&state, &json!({"id": id, "enabled": false}), &c);
    assert!(res.success, "更新成功: {:?}", res.error);
    assert_eq!(res.data.unwrap()["enabled"], false, "enabled=false で停止");

    // 行は残る（delete と違い列挙に出続ける）。
    let got = get_my_schedules(&state, &json!({}), &c);
    let gd = got.data.unwrap();
    assert_eq!(gd["count"], 1, "止めても行は残る（履歴が追える）");
    assert_eq!(gd["schedules"][0]["enabled"], false);
}

/// update で cron を変えると「間隔を変える」が実現し、id は変わらない。
// #654: nostr セッションで作成→更新する。NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn update_changes_interval_same_id() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let id = create_one(&state, &c);

    let res = update_my_schedule(&state, &json!({"id": id, "cron_expr": "0 7 * * *"}), &c);
    assert!(res.success, "更新成功: {:?}", res.error);
    let data = res.data.unwrap();
    assert_eq!(
        data["id"].as_i64().unwrap(),
        id,
        "同じ id を更新（付け替えない）"
    );
    assert_eq!(data["cron_expr"], "0 7 * * *");
}

/// 変更フィールドが 1 つも無い update は暗黙の no-op を避けて拒否する。
// #654: nostr セッションで作成→更新する。NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn update_rejects_no_fields() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let id = create_one(&state, &c);
    let res = update_my_schedule(&state, &json!({"id": id}), &c);
    assert!(!res.success);
    assert!(res.error.unwrap().contains("変更する項目"));
}

/// update の cron 不正は同ターンでエラー（直して呼び直せる）。
// #654: nostr セッションで作成→更新する。NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn update_rejects_invalid_cron_in_the_same_turn() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let id = create_one(&state, &c);
    let res = update_my_schedule(
        &state,
        &json!({"id": id, "cron_expr": "totally not cron"}),
        &c,
    );
    assert!(!res.success);
    assert!(res.error.unwrap().contains("不正"), "cron 不正 remedy");
}

/// delete は行ごと消す（以後 list に出ない）。
// #654: nostr セッションで作成→削除する。NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn delete_removes_row() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let id = create_one(&state, &c);

    let res = delete_my_schedule(&state, &json!({"id": id}), &c);
    assert!(res.success, "削除成功: {:?}", res.error);
    assert_eq!(res.data.unwrap()["id"].as_i64().unwrap(), id);

    let got = get_my_schedules(&state, &json!({}), &c);
    assert_eq!(got.data.unwrap()["count"], 0, "削除後は列挙に出ない");
}

/// 存在しない id の delete は remedy 付きエラー（成功しない）。
// #654: nostr セッションで「見つからない」まで到達するには NostrFire（nostr feature）が要る
// （#651）。off では発火経路解決が先に fail-closed になり所属チェックへ届かないので同じ cfg で囲む。
#[cfg(any())]
#[tokio::test]
async fn delete_missing_id_fails() {
    let state = crate::test_app_state();
    let res = delete_my_schedule(&state, &json!({"id": 999999}), &ctx("nostr-agent-x"));
    assert!(!res.success);
    assert!(res.error.unwrap().contains("見つかりません"));
}

/// **所属チェック（#477 決定事項 1）**: 他エージェント（agent-y）が agent-x の id を推測して
/// 渡しても、update / delete は失敗し、agent-x の行は無傷で残る。
///
/// このテストは所属チェックの変異検出用: `load_owned_schedule` の agent_id 一致条件を外すと
/// delete が通り、`victim_survives` が赤くなる。
// #654: 両者とも nostr セッション（NostrFire・nostr feature）で作成・攻撃する（#651）。
#[cfg(any())]
#[tokio::test]
async fn foreign_agent_cannot_touch_others_schedule() {
    let state = crate::test_app_state();
    let victim = ctx("nostr-agent-x"); // agent-x
    let id = create_one(&state, &victim);

    // agent-y が自分のセッション（発火経路あり）から victim の id を渡す。
    let attacker = ctx_for("agent-y", "nostr-agent-y");

    let del = delete_my_schedule(&state, &json!({"id": id}), &attacker);
    assert!(!del.success, "他エージェントの id は削除できない");
    assert!(
        del.error.unwrap().contains("見つかりません"),
        "存在を明かさない文言"
    );

    let upd = update_my_schedule(&state, &json!({"id": id, "enabled": false}), &attacker);
    assert!(!upd.success, "他エージェントの id は更新できない");

    // victim の行は無傷（削除も更新もされていない）。
    let got = get_my_schedules(&state, &json!({}), &victim);
    let gd = got.data.unwrap();
    assert_eq!(gd["count"], 1, "victim の行は残っている");
    assert_eq!(
        gd["schedules"][0]["enabled"], true,
        "victim の行は更新されていない"
    );
}

/// **セッション所属チェック**: 同じ agent でも別セッション（この agent の Discord チャンネル）
/// からは、Nostr セッションの id を触れない。`load_owned_schedule` の session_id 一致条件を
/// 外すとこのテストが赤くなる。
// #654: nostr セッションで作成し、別セッションからの操作を弾く。作成・照会に NostrFire
// （nostr feature）が要る（#651）。攻撃側の別セッション拒否は理由を問わないので nostr 単独で足りる。
#[cfg(any())]
#[tokio::test]
async fn other_session_of_same_agent_cannot_touch() {
    let state = crate::test_app_state();
    let nostr = ctx("nostr-agent-x");
    let id = create_one(&state, &nostr);

    // 同じ agent-x だが別セッション（Discord）。発火経路はあるが所属が違う。
    let discord = ctx("discord-agent-x-111-222");

    let del = delete_my_schedule(&state, &json!({"id": id}), &discord);
    assert!(!del.success, "別セッションからは削除できない");

    // Nostr 側の行は残る。
    let got = get_my_schedules(&state, &json!({}), &nostr);
    assert_eq!(got.data.unwrap()["count"], 1);
}

/// id を文字列で渡しても受け付ける（LLM が数値を文字列化する実測に対応）。
// #654: nostr セッションで作成→更新する。NostrFire（nostr feature）が要る（#651）。
#[cfg(any())]
#[tokio::test]
async fn update_accepts_stringified_id() {
    let state = crate::test_app_state();
    let c = ctx("nostr-agent-x");
    let id = create_one(&state, &c);
    let res = update_my_schedule(&state, &json!({"id": id.to_string(), "enabled": false}), &c);
    assert!(res.success, "文字列 id を受ける: {:?}", res.error);
}
