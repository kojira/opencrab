//! `setup_status`: the gateway step follows the live gate connections and the conversation
//! step follows the agent's own speech.

use std::sync::Arc;

use opencrab_extgate::registry::{ExtgateState, LiveEntry};
use opencrab_extgate::FinalDelivery;

use super::setup_status;

const AGENT: &str = "crab";
const INSTANCE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

fn state_with_agent_and_instance() -> crate::AppState {
    let mut state = crate::test_app_state();
    state.config.update(|cfg| {
        cfg.gate.listen_socket = "/tmp/example-gate.sock".to_string();
    });
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: AGENT.into(),
                name: "Crab".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "Crab".into(),
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
                [AGENT],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO gate_instances
             (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
             VALUES (?1, 'gateway', ?2, 1, 1, 'e30=', ?3, 1, 1)",
            rusqlite::params![INSTANCE, subject_id, "0".repeat(64)],
        )
        .unwrap();
    }
    state.extgate = Some(Arc::new(ExtgateState::new(state.db.clone())));
    state
}

/// Puts the instance into the live registry (as `hello` does) and returns the peer end that
/// keeps the connection open.
fn connect(state: &crate::AppState) -> tokio::net::UnixStream {
    let (lane, peer) = tokio::net::UnixStream::pair().unwrap();
    let (_read, write) = lane.into_split();
    state
        .extgate
        .as_ref()
        .unwrap()
        .lock_registry()
        .unwrap()
        .insert(
            INSTANCE.to_string(),
            LiveEntry {
                identity: 1,
                writer: Arc::new(tokio::sync::Mutex::new(write)),
                pending: Default::default(),
                declarations: Arc::new(Vec::new()),
                declaration_digest: String::new(),
                final_delivery: FinalDelivery::Automatic,
                run_env: Default::default(),
            },
        );
    peer
}

fn speech(state: &crate::AppState, speaker: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_session_log(
        &conn,
        &opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: AGENT.into(),
            session_id: "extgate-session-1".into(),
            log_type: "speech".into(),
            content: "hello".into(),
            speaker_id: Some(speaker.into()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap();
}

#[tokio::test]
async fn gateway_step_is_not_done_without_a_live_connection() {
    let state = state_with_agent_and_instance();
    let status = setup_status(&state);
    let gateway = &status["steps"]["gateway"];
    assert_eq!(gateway["done"], false);
    assert_eq!(gateway["connected"], serde_json::json!([]));
    assert_eq!(gateway["gate_socket"], "/tmp/example-gate.sock");
    assert_eq!(
        status["steps"]["agent"]["agent_ids"],
        serde_json::json!([AGENT])
    );
    assert_eq!(status["complete"], false);
}

#[tokio::test]
async fn gateway_step_is_done_while_an_instance_is_connected() {
    let state = state_with_agent_and_instance();
    let _peer = connect(&state);
    let gateway = &setup_status(&state)["steps"]["gateway"];
    assert_eq!(gateway["done"], true);
    assert_eq!(
        gateway["connected"],
        serde_json::json!([{ "instance_id": INSTANCE, "agent_id": AGENT }])
    );
}

#[tokio::test]
async fn conversation_step_needs_the_agents_own_speech() {
    let state = state_with_agent_and_instance();
    speech(&state, "local-operator");
    let conversation = &setup_status(&state)["steps"]["conversation"];
    assert_eq!(
        conversation["done"], false,
        "an inbound message alone is not a reply"
    );

    speech(&state, AGENT);
    let conversation = &setup_status(&state)["steps"]["conversation"];
    assert_eq!(conversation["done"], true);
    assert_eq!(conversation["agent_id"], AGENT);
    assert_eq!(conversation["session_id"], "extgate-session-1");
}

#[tokio::test]
async fn next_step_is_the_first_step_not_done() {
    let state = state_with_agent_and_instance();
    // The test state's default provider ("mock") is not usable, so setup starts there.
    assert_eq!(setup_status(&state)["next_step"], "llm_provider");
    state
        .config
        .update(|cfg| cfg.llm.default_provider = "codex".to_string());
    assert_eq!(setup_status(&state)["next_step"], "gateway");
    let _peer = connect(&state);
    assert_eq!(setup_status(&state)["next_step"], "conversation");
    speech(&state, AGENT);
    let status = setup_status(&state);
    assert_eq!(status["next_step"], serde_json::Value::Null);
    assert_eq!(status["complete"], true);
}
