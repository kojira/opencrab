#![cfg(test)]
//! D-1086: session reasoning effort administration on `AppState`.

use opencrab_actions::{ModelAdminError, ModelAdministration, TurnModelProfile};
use opencrab_db::queries::{get_session_reasoning_effort, ReasoningEffortSource, SessionEffortSlot};

fn state() -> crate::AppState {
    let state = crate::test_app_state();
    {
        let conn = state.db.lock().unwrap();
        for (agent_id, effort) in [("a1", Some("high")), ("a2", None)] {
            opencrab_db::queries::upsert_agent(
                &conn,
                &opencrab_db::queries::AgentRow {
                    agent_id: agent_id.to_string(),
                    name: agent_id.to_string(),
                    job_title: None,
                    organization: None,
                    image_url: None,
                    persona_name: agent_id.to_string(),
                    personality: None,
                    instructions: String::new(),
                    model: None,
                    reasoning_effort: effort.map(str::to_string),
                    web_search: None,
                    metadata_json: None,
                },
            )
            .unwrap();
        }
        for (id, members) in [("sess-a", &["a1"][..]), ("sess-shared", &["a1", "a2"][..])] {
            opencrab_db::queries::insert_session(
                &conn,
                &opencrab_db::queries::SessionRow {
                    id: id.to_string(),
                    mode: "generic".to_string(),
                    theme: id.to_string(),
                    phase: "active".to_string(),
                    turn_number: 0,
                    status: "active".to_string(),
                    participant_ids_json: serde_json::json!(members).to_string(),
                    facilitator_id: None,
                    done_count: 0,
                    max_turns: None,
                    metadata_json: None,
                },
            )
            .unwrap();
        }
    }
    state
}

fn stored(state: &crate::AppState, slot: SessionEffortSlot) -> Option<String> {
    let conn = state.db.lock().unwrap();
    get_session_reasoning_effort(&conn, "sess-a", slot).unwrap()
}

/// Set/list/reset report each slot's configured and effective value with its source; a value
/// outside none/low/medium/high is `InvalidArgs` and a session that is not the agent's own is
/// `TargetUnavailable`, with nothing written.
#[tokio::test]
async fn session_reasoning_effort_set_list_reset_and_rejections() {
    let state = state();
    let list = state.list_session_reasoning_efforts("a1", "sess-a").await.unwrap();
    assert_eq!(list.default.current_effort.as_deref(), Some("high"));
    assert_eq!(list.default.source, ReasoningEffortSource::Agent);
    assert_eq!(list.realtime.current_effort, None);
    assert_eq!(list.realtime.source, ReasoningEffortSource::Provider);

    let rt = state
        .set_session_reasoning_effort("a1", "sess-a", TurnModelProfile::Realtime, "low")
        .await
        .unwrap();
    assert_eq!(rt.configured_effort.as_deref(), Some("low"));
    assert_eq!(rt.current_effort.as_deref(), Some("low"));
    assert_eq!(rt.source, ReasoningEffortSource::Session);
    assert_eq!(stored(&state, SessionEffortSlot::Realtime).as_deref(), Some("low"));
    assert_eq!(stored(&state, SessionEffortSlot::Default), None);

    for bad in ["minimal", "xhigh", "", " low", "LOW"] {
        assert_eq!(
            state
                .set_session_reasoning_effort("a1", "sess-a", TurnModelProfile::Default, bad)
                .await,
            Err(ModelAdminError::InvalidArgs),
            "{bad:?}"
        );
    }
    assert_eq!(
        state
            .set_session_reasoning_effort("a1", "sess-shared", TurnModelProfile::Default, "low")
            .await,
        Err(ModelAdminError::TargetUnavailable)
    );
    assert_eq!(
        state
            .set_session_reasoning_effort("a1", "missing", TurnModelProfile::Default, "low")
            .await,
        Err(ModelAdminError::TargetUnavailable)
    );
    assert_eq!(stored(&state, SessionEffortSlot::Default), None);

    let reset = state
        .reset_session_reasoning_effort("a1", "sess-a", TurnModelProfile::Realtime)
        .await
        .unwrap();
    assert_eq!(reset.configured_effort, None);
    assert_eq!(reset.source, ReasoningEffortSource::Provider);
    assert_eq!(stored(&state, SessionEffortSlot::Realtime), None);
}
