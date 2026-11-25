//! D-1150: 一回だけの予定（`@at`）は scheduler の同じ発火ループで 1 回だけ発火し、発火の記録と同じ
//! UPDATE で無効になる。発火先は行のセッション（会話中のセッションとは別の、自分のセッション）。
use super::*;

use std::sync::{Arc, Mutex};

use opencrab_actions::{TimedFireRequest, TimedFireSink};

struct CollectingSink {
    requests: Arc<Mutex<Vec<TimedFireRequest>>>,
    delivered: Arc<tokio::sync::Notify>,
}

impl TimedFireSink for CollectingSink {
    fn fire_timed_turn(&self, request: TimedFireRequest) {
        self.requests.lock().unwrap().push(request);
        self.delivered.notify_one();
    }
}

fn one_shot_row(session_id: &str, at: DateTime<Utc>) -> AgentScheduleRow {
    AgentScheduleRow {
        id: None,
        agent_id: AGENT_UUID.into(),
        session_id: session_id.into(),
        cron_expr: format!("@at {}", at.to_rfc3339()),
        timezone: "Asia/Tokyo".into(),
        message: "one-shot message".into(),
        enabled: true,
        anchor_at: Some(Utc::now().to_rfc3339()),
        last_fired_at: None,
    }
}

/// 時刻が来たら行のセッションで 1 回発火し、無効になり、その後は発火しない。
#[tokio::test]
async fn one_shot_fires_once_on_its_session_and_is_disabled() {
    let mock = Arc::new(crate::bin_test_support::FixedTextMock::new("NO_REPLY"));
    let state = crate::bin_test_support::app_state_with_agent(mock, AGENT_UUID);
    let other_session = "opaque-other-session";
    let schedule_id = {
        let mut conn = state.db.lock().unwrap();
        let (instance_id, _) =
            seed_generic_alias_binding(&mut conn, AGENT_UUID, "opaque-talk-session");
        let tx = conn.transaction().unwrap();
        insert_owned_gate_session(
            &tx,
            AGENT_UUID,
            "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            &instance_id,
            other_session,
            other_session,
        );
        tx.commit().unwrap();
        let at = Utc::now() + Duration::milliseconds(1500);
        opencrab_db::queries::insert_agent_schedule(&conn, &one_shot_row(other_session, at))
            .unwrap()
    };

    let requests = Arc::new(Mutex::new(Vec::new()));
    let delivered = Arc::new(tokio::sync::Notify::new());
    state
        .timed_fire_router
        .register_sink(Arc::new(CollectingSink {
            requests: Arc::clone(&requests),
            delivered: Arc::clone(&delivered),
        }));
    let scheduler = tokio::spawn(run_scheduler(state.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(5), delivered.notified())
        .await
        .expect("one-shot did not fire at its time");
    // 発火後もしばらく回して、二度目が来ないことを見る。
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    state.scheduler_wake.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    scheduler.abort();
    let _ = scheduler.await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "一回だけ発火する");
    assert_eq!(requests[0].session_id, other_session);
    assert!(requests[0].prompt.contains("one-shot message"));
    drop(requests);

    let conn = state.db.lock().unwrap();
    let row = opencrab_db::queries::get_agent_schedule(&conn, schedule_id)
        .unwrap()
        .unwrap();
    assert!(!row.enabled, "発火すると scheduler が無効にする");
    assert!(row.last_fired_at.is_some());
    assert!(rebuild_entries(&test_router(), &conn, &HashMap::new()).is_empty());
}

/// 停止中に時刻が過ぎた一回だけの予定は、cron の missed-run と同じく復帰後に 1 回だけ due になる。
#[test]
fn one_shot_missed_while_down_is_due_once() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let sid = format!("nostr-{AGENT_UUID}");
    seed_generic_alias_binding(&mut conn, AGENT_UUID, &sid);
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &one_shot_row(&sid, Utc::now() - Duration::hours(3)),
    )
    .unwrap();
    let entries = rebuild_entries(&test_router(), &conn, &HashMap::new());
    assert_eq!(entries.len(), 1);
    assert!(entries[0].next_fire_at.unwrap() <= Utc::now(), "due");
    assert!(entries[0].one_shot_expr.is_some());
}
