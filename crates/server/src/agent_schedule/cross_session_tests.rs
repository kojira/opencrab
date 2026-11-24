//! D-1147: エージェントは自分の全セッションのスケジュールを見て・変えられる。他エージェントの id は
//! 存在を明かさず拒否する。サブタスクからも親と同じ範囲で扱える。
use super::*;
use serde_json::json;

const NOT_FOUND: &str = "指定した id のスケジュールが見つかりません";

/// `agent_id` のエージェントとゲートインスタンスを作り、`session_ids` をそのゲートの会話として登録する
/// （`resolve_persisted_target` が発火経路ありと判定する状態）。
fn bind_sessions(state: &AppState, agent_id: &str, session_ids: &[(&str, &str)]) {
    let mut conn = state.db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: agent_id.into(),
            name: agent_id.into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "persona".into(),
            personality: None,
            instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [agent_id],
            |row| row.get(0),
        )
        .unwrap();
    let instance_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'generic', ?2, 1, 1, 'e30=', ?3, 1, 1)",
        rusqlite::params![instance_id, subject_id, "0".repeat(64)],
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    for (session_id, title) in session_ids {
        opencrab_db::queries::insert_session_in_tx(&tx, session_id, title, "2026-01-01T00:00:00Z")
            .unwrap();
        opencrab_db::queries::insert_agent_session_in_tx(&tx, agent_id, session_id).unwrap();
        tx.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at, closed_at, session_id)
             VALUES (?1, ?2, ?3, 1, NULL, ?3)",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), instance_id, session_id],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn context(caller: GatewayCaller, agent_id: &str, session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(caller, agent_id).with_session_id(session_id.to_string())
}

fn trusted(session_id: &str) -> GatewayCallContext {
    context(GatewayCaller::TrustedUser, "agent-x", session_id)
}

fn create(state: &AppState, ctx: &GatewayCallContext, message: &str) -> i64 {
    let res = set_my_schedule(
        state,
        &json!({"cron_expr": "@every 3h", "message": message}),
        ctx,
    );
    assert!(res.success, "create: {:?}", res.error);
    res.data.unwrap()["id"].as_i64().unwrap()
}

/// agent-x: 公開会話 `sess-public`（名前あり）と DM `sess-dm`（名前なし）。agent-y: `sess-other`。
fn two_agent_state() -> AppState {
    let state = crate::test_app_state();
    bind_sessions(
        &state,
        "agent-x",
        &[("sess-public", "public timeline"), ("sess-dm", "sess-dm")],
    );
    bind_sessions(&state, "agent-y", &[("sess-other", "other agent room")]);
    state
}

fn row(state: &AppState, id: i64) -> Option<opencrab_db::queries::AgentScheduleRow> {
    opencrab_db::queries::get_agent_schedule(&state.db.lock().unwrap(), id).unwrap()
}

#[test]
fn get_my_schedules_lists_schedules_of_all_own_sessions() {
    let state = two_agent_state();
    let public_id = create(&state, &trusted("sess-public"), "patrol public");
    create(
        &state,
        &context(GatewayCaller::TrustedUser, "agent-y", "sess-other"),
        "not mine",
    );

    let got = get_my_schedules(&state, &json!({}), &trusted("sess-dm"));
    assert!(got.success, "list: {:?}", got.error);
    let data = got.data.unwrap();
    assert_eq!(data["current_session_id"], "sess-dm");
    assert_eq!(data["count"], 1, "他エージェントの行は出ない: {data}");
    let item = &data["schedules"][0];
    assert_eq!(item["id"].as_i64().unwrap(), public_id);
    assert_eq!(item["session_id"], "sess-public");
    assert_eq!(item["session_title"], "public timeline");
    assert_eq!(item["is_current_session"], false);

    let filtered = get_my_schedules(
        &state,
        &json!({"session_id": "sess-dm"}),
        &trusted("sess-dm"),
    );
    assert_eq!(
        filtered.data.unwrap()["count"],
        0,
        "session_id で絞り込める"
    );
}

#[test]
fn update_delete_and_run_work_on_another_own_session() {
    let state = two_agent_state();
    let id = create(&state, &trusted("sess-public"), "patrol public");

    let updated = update_my_schedule(
        &state,
        &json!({"id": id, "message": "patrol updated"}),
        &trusted("sess-dm"),
    );
    assert!(updated.success, "update: {:?}", updated.error);
    let data = updated.data.unwrap();
    assert_eq!(data["message"], "patrol updated");
    assert_eq!(
        data["session_id"], "sess-public",
        "発火先は行のセッションのまま"
    );

    let owner = context(GatewayCaller::Owner, "agent-x", "sess-dm");
    let run = run_my_schedule(&state, &json!({"id": id}), &owner);
    // sink 未登録のテスト状態では「ゲートウェイが稼働していない」で止まる＝所属チェックと
    // 発火先の解決（行のセッション）は通っている。
    assert!(!run.success);
    let e = run.error.unwrap();
    assert!(!e.contains(NOT_FOUND), "所属チェックは通る: {e}");
    assert!(e.contains("ゲートウェイが稼働していない"), "{e}");

    let deleted = delete_my_schedule(&state, &json!({"id": id}), &trusted("sess-dm"));
    assert!(deleted.success, "delete: {:?}", deleted.error);
    assert!(row(&state, id).is_none());
}

#[test]
fn other_agents_id_is_refused_like_a_missing_id() {
    let state = two_agent_state();
    let foreign = create(
        &state,
        &context(GatewayCaller::TrustedUser, "agent-y", "sess-other"),
        "agent-y patrol",
    );
    let me = trusted("sess-dm");
    let owner = context(GatewayCaller::Owner, "agent-x", "sess-dm");
    let missing = update_my_schedule(
        &state,
        &json!({"id": foreign + 1000, "enabled": false}),
        &me,
    )
    .error
    .unwrap();
    assert!(missing.contains(NOT_FOUND));

    let upd = update_my_schedule(&state, &json!({"id": foreign, "enabled": false}), &me);
    let del = delete_my_schedule(&state, &json!({"id": foreign}), &me);
    let run = run_my_schedule(&state, &json!({"id": foreign}), &owner);
    for res in [upd, del, run] {
        assert!(!res.success);
        assert_eq!(res.error.unwrap(), missing, "存在を明かさない同じ文言");
    }
    let kept = row(&state, foreign).expect("agent-y の行は残る");
    assert!(kept.enabled, "agent-y の行は変わらない");
}

#[test]
fn set_my_schedule_registers_on_another_own_session() {
    let state = two_agent_state();
    let res = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "from dm", "session_id": "sess-public"}),
        &trusted("sess-dm"),
    );
    assert!(res.success, "set with session_id: {:?}", res.error);
    assert_eq!(res.data.unwrap()["session_id"], "sess-public");
}

#[test]
fn set_my_schedule_refuses_foreign_or_unknown_session() {
    let state = two_agent_state();
    for session_id in ["sess-other", "sess-unknown"] {
        let res = set_my_schedule(
            &state,
            &json!({"cron_expr": "@every 3h", "message": "x", "session_id": session_id}),
            &trusted("sess-dm"),
        );
        assert!(!res.success, "{session_id} は登録できない");
        assert!(res.error.unwrap().contains("定時実行を設定できません"));
    }
    let conn = state.db.lock().unwrap();
    assert!(opencrab_db::queries::list_agent_schedules(&conn, "agent-x")
        .unwrap()
        .is_empty());
    assert_eq!(
        opencrab_db::queries::list_agent_schedules(&conn, "agent-y")
            .unwrap()
            .len(),
        0
    );
}

/// 本番で起きた形: スケジュールは公開会話にあり、DM 会話で起こしたサブタスクから見て・変える。
/// サブタスクは親の呼び出し元（ここでは TrustedUser）を継承する（`subtask_spawn::spawn`）。
#[tokio::test]
async fn subtask_can_list_and_update_parent_agents_schedules() {
    use opencrab_gateway::GatewayActions;
    let state = two_agent_state();
    let id = create(&state, &trusted("sess-public"), "patrol public");

    let registry: opencrab_actions::SubtaskRegistry = std::sync::Arc::new(dashmap::DashMap::new());
    registry.insert(
        "st-1".to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: "subtask-st-1".to_string(),
            parent_session_id: "sess-dm".to_string(),
            agent_id: "agent-x".to_string(),
            label: "job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::TrustedUser,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    let actions =
        crate::system_actions::SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let sub = trusted("subtask-st-1").with_depth(1);

    let got = actions.execute("get_my_schedules", &json!({}), &sub).await;
    assert!(got.success, "subtask list: {:?}", got.error);
    let data = got.data.unwrap();
    assert_eq!(data["current_session_id"], "sess-dm", "この会話は親の会話");
    assert_eq!(data["count"], 1);
    assert_eq!(data["schedules"][0]["session_id"], "sess-public");

    let updated = actions
        .execute(
            "update_my_schedule",
            &json!({"id": id, "enabled": false}),
            &sub,
        )
        .await;
    assert!(updated.success, "subtask update: {:?}", updated.error);
    assert!(!row(&state, id).unwrap().enabled);
}
