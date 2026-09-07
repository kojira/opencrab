use super::*;

const AGENT: &str = "11111111-1111-4111-8111-111111111111";

fn state_with_db() -> AppState {
    crate::test_app_state()
}

fn nostr_session() -> String {
    format!("nostr-{AGENT}")
}

#[tokio::test]
async fn create_rejects_bad_cron() {
    let state = state_with_db();
    let req = CreateRequest {
        session_id: nostr_session(),
        cron_expr: "not a cron".to_string(),
        timezone: "Asia/Tokyo".to_string(),
        message: "hi".to_string(),
        enabled: true,
    };
    let res = create_schedule(State(state), Path(AGENT.to_string()), Json(req)).await;
    assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
}

#[tokio::test]
async fn create_rejects_foreign_session() {
    let state = state_with_db();
    // 別エージェントの session を渡す → resolve が None → 400。
    let req = CreateRequest {
        session_id: "nostr-other-agent".to_string(),
        cron_expr: "0 7 * * *".to_string(),
        timezone: "Asia/Tokyo".to_string(),
        message: "hi".to_string(),
        enabled: true,
    };
    let res = create_schedule(State(state), Path(AGENT.to_string()), Json(req)).await;
    assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
}

// #654: nostr セッションで作成する。resolve_target は NostrFire descriptor（nostr feature）が
// 要る（#651）。off では作成が 400/fail-closed になり検証対象の挙動が存在しないので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn create_then_list_computes_next_fire_at() {
    let state = state_with_db();
    let req = CreateRequest {
        session_id: nostr_session(),
        cron_expr: "@every 3h".to_string(),
        timezone: "Asia/Tokyo".to_string(),
        message: "巡回してください".to_string(),
        enabled: true,
    };
    let created = create_schedule(State(state.clone()), Path(AGENT.to_string()), Json(req))
        .await
        .expect("create ok")
        .0;
    assert!(created.id > 0);
    assert!(created.enabled);
    assert!(created.anchor_at.is_some(), "enabled 作成で anchor=now");
    // @every 3h・anchor=now → next_fire_at は算出され未来（now+3h）。
    assert!(
        created.next_fire_at.is_some(),
        "next_fire_at が照会時算出される（列に持たない）"
    );

    let listed = list_schedules(State(state), Path(AGENT.to_string()))
        .await
        .expect("list ok")
        .0;
    assert_eq!(listed.count, 1);
    assert_eq!(listed.schedules[0].id, created.id);
}

// #654: nostr セッションで作成→更新→削除する。NostrFire（nostr feature）が要る（#651）。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn patch_disable_stops_and_keeps_phase_then_delete() {
    let state = state_with_db();
    let created = create_schedule(
        State(state.clone()),
        Path(AGENT.to_string()),
        Json(CreateRequest {
            session_id: nostr_session(),
            cron_expr: "@every 3h".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "x".to_string(),
            enabled: true,
        }),
    )
    .await
    .unwrap()
    .0;
    let anchor_before = created.anchor_at.clone();

    // enabled=false で停止。無効化では anchor を触らない（位相保存）。
    let patched = update_schedule(
        State(state.clone()),
        Path(created.id),
        Json(PatchRequest {
            session_id: None,
            cron_expr: None,
            timezone: None,
            message: None,
            enabled: Some(false),
        }),
    )
    .await
    .unwrap()
    .0;
    assert!(!patched.enabled, "enabled=false で停止");
    assert_eq!(
        patched.anchor_at, anchor_before,
        "無効化で anchor を触らない"
    );

    // 削除 → 以後 list は空。
    let _ = delete_schedule(State(state.clone()), Path(created.id))
        .await
        .expect("delete ok");
    let listed = list_schedules(State(state), Path(AGENT.to_string()))
        .await
        .unwrap()
        .0;
    assert_eq!(listed.count, 0);
}

// #654: nostr セッションで作成→cron 更新する。NostrFire（nostr feature）が要る（#651）。
#[cfg(feature = "nostr")]
#[tokio::test]
async fn patch_cron_change_resets_anchor() {
    let state = state_with_db();
    let created = create_schedule(
        State(state.clone()),
        Path(AGENT.to_string()),
        Json(CreateRequest {
            session_id: nostr_session(),
            cron_expr: "@every 3h".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "x".to_string(),
            enabled: true,
        }),
    )
    .await
    .unwrap()
    .0;

    // last_fired を刻んでおく（発火済みを模す）。
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_schedule_last_fired(
            &conn,
            created.id,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    }

    // cron 式を変更 → anchor=now・last_fired=NULL にリセットされる（明示変更）。
    let patched = update_schedule(
        State(state),
        Path(created.id),
        Json(PatchRequest {
            session_id: None,
            cron_expr: Some("0 7 * * *".to_string()),
            timezone: None,
            message: None,
            enabled: None,
        }),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(patched.cron_expr, "0 7 * * *");
    assert_eq!(
        patched.last_fired_at, None,
        "cron 変更で last_fired をリセット（新しい式で次スロットから）"
    );
    assert!(patched.anchor_at.is_some(), "cron 変更で anchor=now");
}

#[tokio::test]
async fn patch_missing_is_404() {
    let state = state_with_db();
    let res = update_schedule(
        State(state),
        Path(99999),
        Json(PatchRequest {
            session_id: None,
            cron_expr: None,
            timezone: None,
            message: Some("x".to_string()),
            enabled: None,
        }),
    )
    .await;
    assert_eq!(res.err(), Some(StatusCode::NOT_FOUND));
}

// ---- 冪等性（同じ内容の再登録で二重発火しない・マージ前修正） ----

// #654: nostr セッションで作成する。create_schedule_core の resolve_target は NostrFire
// （nostr feature）が要る（#651）。off では .expect が落ちるので同じ cfg で囲む。
#[cfg(feature = "nostr")]
#[test]
fn create_is_idempotent_on_same_content() {
    let state = state_with_db();
    let s = nostr_session();
    let a = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
        .expect("1st ok");
    let b = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
        .expect("2nd ok");
    assert_eq!(a.id, b.id, "同一内容の再登録は同じ id を返す（冪等）");
    // 行は 1 本だけ。
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_agent_schedules(&conn, AGENT).unwrap();
    assert_eq!(rows.len(), 1, "同一内容は 1 本のまま（二重発火しない）");
}

// #654: nostr セッションで作成する。NostrFire（nostr feature）が要る（#651）。
#[cfg(feature = "nostr")]
#[test]
fn same_cron_different_message_is_two_schedules() {
    let state = state_with_db();
    let s = nostr_session();
    let a = create_schedule_core(&state, AGENT, &s, "0 7 * * *", "Asia/Tokyo", "まとめ", true)
        .expect("a");
    let b = create_schedule_core(&state, AGENT, &s, "0 7 * * *", "Asia/Tokyo", "巡回", true)
        .expect("b");
    assert_ne!(a.id, b.id, "同じ cron でも message が違えば別スケジュール");
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_agent_schedules(&conn, AGENT).unwrap();
    assert_eq!(rows.len(), 2, "cron だけで dedup しない");
}

/// 既に有効な同一内容の再登録では位相（anchor/last_fired）を保存する（冪等 = 時刻も動かさない）。
// #654: nostr セッションで作成する。NostrFire（nostr feature）が要る（#651）。
#[cfg(feature = "nostr")]
#[test]
fn idempotent_reregister_preserves_phase() {
    let state = state_with_db();
    let s = nostr_session();
    let a = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
        .expect("a");
    // last_fired を刻んでおく（発火済みを模す）。
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_schedule_last_fired(&conn, a.id, "2026-08-09T07:00:00Z")
            .unwrap();
    }
    let b = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
        .expect("b");
    assert_eq!(a.id, b.id);
    assert_eq!(
        b.last_fired_at.as_deref(),
        Some("2026-08-09T07:00:00Z"),
        "有効な同一内容の再登録は位相を保存する（次回発火が動かない＝冪等）"
    );
}
