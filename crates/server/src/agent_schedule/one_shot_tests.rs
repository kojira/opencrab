//! D-1150: 一回だけの予定（`at`）。同じ `agent_schedules` の行・同じ scheduler・同じ発火経路を使い、
//! 発火すると scheduler が無効にする。別の自分のセッションも対象にできる。
use super::cross_session_tests::{row, trusted, two_agent_state};
use super::*;
use chrono::{Duration, Utc};
use serde_json::json;

fn local_minute(t: chrono::DateTime<Utc>) -> String {
    t.with_timezone(&chrono_tz::Asia::Tokyo)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

#[test]
fn one_shot_for_another_own_session_is_listed_and_can_be_moved() {
    let state = two_agent_state();
    let at = Utc::now() + Duration::days(1);
    let res = set_my_schedule(
        &state,
        &json!({"at": local_minute(at), "message": "post once", "session_id": "sess-public"}),
        &trusted("sess-dm"),
    );
    assert!(res.success, "set: {:?}", res.error);
    let data = res.data.unwrap();
    let id = data["id"].as_i64().unwrap();
    assert_eq!(data["session_id"], "sess-public");
    assert_eq!(data["one_shot"], true);
    assert!(data["cron_expr"].as_str().unwrap().starts_with("@at "));

    let listed = get_my_schedules(&state, &json!({}), &trusted("sess-dm"));
    let item = &listed.data.unwrap()["schedules"][0];
    assert_eq!(item["one_shot"], true);
    let next = chrono::DateTime::parse_from_rfc3339(item["next_fire_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        local_minute(next),
        local_minute(at),
        "next_fire_at は指定した時刻"
    );

    let later = at + Duration::hours(2);
    let moved = update_my_schedule(
        &state,
        &json!({"id": id, "at": local_minute(later)}),
        &trusted("sess-dm"),
    );
    assert!(moved.success, "move: {:?}", moved.error);
    let next = moved.data.unwrap()["next_fire_at"]
        .as_str()
        .unwrap()
        .to_string();
    let next = chrono::DateTime::parse_from_rfc3339(&next)
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(local_minute(next), local_minute(later));
    assert_eq!(row(&state, id).unwrap().session_id, "sess-public");
}

#[test]
fn past_or_ambiguous_one_shot_is_refused() {
    let state = two_agent_state();
    let past = set_my_schedule(
        &state,
        &json!({"at": local_minute(Utc::now() - Duration::minutes(5)), "message": "late"}),
        &trusted("sess-dm"),
    );
    assert!(!past.success);
    assert!(past.error.unwrap().contains("既に過ぎて"));

    let both = set_my_schedule(
        &state,
        &json!({"at": local_minute(Utc::now() + Duration::days(1)), "cron_expr": "@every 3h", "message": "x"}),
        &trusted("sess-dm"),
    );
    assert!(!both.success);
    assert!(both.error.unwrap().contains("同時に指定できません"));

    let neither = set_my_schedule(&state, &json!({"message": "x"}), &trusted("sess-dm"));
    assert!(!neither.success);
    assert!(neither.error.unwrap().contains("cron_expr か at"));
    assert!(
        opencrab_db::queries::list_agent_schedules(&state.db.lock().unwrap(), "agent-x")
            .unwrap()
            .is_empty()
    );
}

struct CollectingSink {
    requests: std::sync::Arc<std::sync::Mutex<Vec<opencrab_actions::TimedFireRequest>>>,
    delivered: std::sync::Arc<tokio::sync::Notify>,
}

impl opencrab_actions::TimedFireSink for CollectingSink {
    fn fire_timed_turn(&self, request: opencrab_actions::TimedFireRequest) {
        self.requests.lock().unwrap().push(request);
        self.delivered.notify_one();
    }
}

/// 手動発火（`run_my_schedule`）は今までどおり `last_fired_at` を触らず、一回だけの予定を消費しない。
#[tokio::test]
async fn run_my_schedule_does_not_consume_a_one_shot() {
    let state = two_agent_state();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let delivered = std::sync::Arc::new(tokio::sync::Notify::new());
    state
        .timed_fire_router
        .register_sink(std::sync::Arc::new(CollectingSink {
            requests: requests.clone(),
            delivered: delivered.clone(),
        }));
    let res = set_my_schedule(
        &state,
        &json!({"at": local_minute(Utc::now() + Duration::days(1)), "message": "once"}),
        &trusted("sess-public"),
    );
    let id = res.data.unwrap()["id"].as_i64().unwrap();
    let owner = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
        .with_session_id("sess-dm".to_string());
    let run = run_my_schedule(&state, &json!({"id": id}), &owner);
    assert!(run.success, "run: {:?}", run.error);
    tokio::time::timeout(std::time::Duration::from_secs(3), delivered.notified())
        .await
        .expect("manual fire reaches the sink");
    assert_eq!(requests.lock().unwrap()[0].session_id, "sess-public");
    let after = row(&state, id).unwrap();
    assert!(after.enabled, "手動発火では無効にならない");
    assert!(after.last_fired_at.is_none());
}
