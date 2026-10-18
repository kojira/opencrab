// D-1085 P5: a provisional run is the realtime single generation on a session snapshot; it is
// labelled `provisional` and writes nothing to the session or the agent.

use opencrab_actions::run_provisional_turn;

/// The provisional run as the external gate starts it: the realtime prompt, the session snapshot
/// at `high_water`, then the pending user line and agent prelude.
async fn provisional_run(
    f: &Fixture,
    high_water: i64,
    pending: &[opencrab_db::queries::SessionLogRow],
) -> opencrab_core::EngineResult {
    provisional_run_with(f, high_water, pending, opencrab_core::PromotionGate::new(), None)
        .await
        .expect("provisional succeeded")
}

/// [`provisional_run`] with its promotion gate and the gateway's declared operations.
async fn provisional_run_with(
    f: &Fixture,
    high_water: i64,
    pending: &[opencrab_db::queries::SessionLogRow],
    gate: opencrab_core::PromotionGate,
    gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>>,
) -> anyhow::Result<opencrab_core::EngineResult> {
    let (system, name) = f.state.build_realtime_context("a1");
    run_provisional_turn(
        &f.state,
        "sess-a",
        "a1",
        &system,
        high_water,
        pending,
        gate,
        |conversation| {
            let req = RunRequest::new(
                "a1",
                name.clone(),
                "sess-a",
                system.clone(),
                conversation,
                "test",
                CallerIdentity::Owner,
            );
            match gateway_actions {
                Some(ga) => req.with_gateway_actions(ga),
                None => req,
            }
        },
    )
    .await
    .expect("provisional ran")
}

fn high_water(f: &Fixture) -> i64 {
    let conn = f.state.db.lock().unwrap();
    opencrab_db::queries::session_log_high_water(&conn, "sess-a").unwrap()
}

fn session_row_count(f: &Fixture) -> i64 {
    f.state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = 'sess-a'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// The conversation is the last speech rows up to the snapshot, then the pending lines; a row
/// recorded after the snapshot is not in it.
#[tokio::test]
async fn provisional_conversation_is_the_snapshot_then_the_pending_lines() {
    let f = fixture();
    *f.reply.lock().unwrap() = "はい。".to_string();
    let hw = high_water(&f);
    {
        let conn = f.state.db.lock().unwrap();
        opencrab_db::queries::insert_session_log(
            &conn,
            &log_row("speech", Some("u2"), "AFTER-SNAPSHOT", None),
        )
        .unwrap();
    }
    let pending = [
        log_row("speech", Some("u1"), "PENDING-USER", Some(r#"{"owner":true}"#)),
        log_row("speech", Some("a1"), "PENDING-PRELUDE", None),
    ];
    provisional_run(&f, hw, &pending).await;

    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "one LLM call");
    let user = user_text(&requests[0]);
    assert!(!user.contains("AFTER-SNAPSHOT"), "{user}");
    assert!(!user.contains("agent-speech-19"), "only the last 20 rows: {user}");
    assert!(user.contains("agent-speech-39"), "{user}");
    let history = user
        .split(opencrab_core::conversation::CONVERSATION_HISTORY_END)
        .next()
        .unwrap();
    let last = history.find("agent-speech-39").unwrap();
    let said = history.find("PENDING-USER").expect("pending user line");
    let prelude = history.find("PENDING-PRELUDE").expect("pending prelude");
    assert!(last < said && said < prelude, "{history}");
}

/// F3: the run is metered as `provisional` and leaves no session row, skill usage or turn-end
/// snapshot.
#[tokio::test]
async fn provisional_run_is_labelled_and_writes_nothing() {
    let f = fixture();
    *f.reply.lock().unwrap() = "visible-skill で答えるね。".to_string();
    let rows_before = session_row_count(&f);
    let pending = [log_row("speech", Some("u1"), "PENDING-USER", None)];
    provisional_run(&f, high_water(&f), &pending).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let conn = f.state.db.lock().unwrap();
    let purposes: Vec<String> = conn
        .prepare("SELECT purpose FROM llm_usage_metrics")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(purposes, ["provisional"]);
    let rows_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = 'sess-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows_after, rows_before);
    let usage: i64 = conn
        .query_row("SELECT usage_count FROM skills WHERE id = 's1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(usage, 0);
    let snapshots: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_snapshots WHERE session_id = 'sess-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(snapshots, 0);
}
