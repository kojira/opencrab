#![cfg(test)]
//! D-1080: session model administration and the pinned model of a session turn.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_actions::{
    run_session_turn, AgentRuntime, CallerIdentity, ModelAdminError, ModelAdministration,
    RunRequest, TurnModelProfile,
};
use opencrab_llm::message::{ChatRequest, ChatResponse, Choice, FinishReason, Message};
use opencrab_llm::traits::{LlmProvider, ModelInfo};

/// Every LLM request as `(model, max_tokens)`.
type Seen = Arc<Mutex<Vec<(String, Option<u32>)>>>;

/// Model catalog `pa:{small,big,nobudget,shared}` + `pb:shared`; records every request.
struct CatalogProvider {
    name: &'static str,
    models: &'static [&'static str],
    seen: Seen,
}

#[async_trait]
impl LlmProvider for CatalogProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn available_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(self
            .models
            .iter()
            .map(|id| ModelInfo {
                id: (*id).to_string(),
                name: (*id).to_string(),
                context_window: 0,
                supports_function_calling: true,
                supports_vision: false,
            })
            .collect())
    }

    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.seen
            .lock()
            .unwrap()
            .push((request.model.clone(), request.max_tokens));
        Ok(ChatResponse {
            id: "r".to_string(),
            model: request.model,
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("ok\nNO_REPLY"),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Default::default(),
            created: 0,
        })
    }
}

fn pricing(conn: &rusqlite::Connection, model: &str, window: i32, max_out: i32) {
    opencrab_db::queries::upsert_model_pricing(
        conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: "pa".to_string(),
            model: model.to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(window),
            max_output_tokens: Some(max_out),
            cached_input_price_per_1m: None,
            cache_write_price_per_1m: None,
        },
    )
    .unwrap();
}

fn agent(conn: &rusqlite::Connection, agent_id: &str, model: Option<&str>) {
    opencrab_db::queries::upsert_agent(
        conn,
        &opencrab_db::queries::AgentRow {
            agent_id: agent_id.to_string(),
            name: agent_id.to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: agent_id.to_string(),
            personality: None,
            instructions: String::new(),
            model: model.map(str::to_string),
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

fn session(conn: &rusqlite::Connection, id: &str, members: &[&str]) {
    opencrab_db::queries::insert_session(
        conn,
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

/// Agent `a1` defaults to `pa:small` (window 50k / out 1k); `pa:big` is 300k / 8k;
/// `pa:nobudget` is available but has no pricing row.
fn state() -> (crate::AppState, Seen) {
    let state = crate::test_app_state();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    {
        let conn = state.db.lock().unwrap();
        pricing(&conn, "small", 50_000, 1_000);
        pricing(&conn, "big", 300_000, 8_000);
        agent(&conn, "a1", Some("pa:small"));
        agent(&conn, "a2", None);
        session(&conn, "sess-a", &["a1"]);
        session(&conn, "sess-b", &["a1"]);
        session(&conn, "sess-shared", &["a1", "a2"]);
        session(&conn, "sess-other", &["a2"]);
    }
    let mut router = opencrab_llm::router::LlmRouter::new();
    router.add_provider(Arc::new(CatalogProvider {
        name: "pa",
        models: &["small", "big", "nobudget", "shared", "light", "huge"],
        seen: seen.clone(),
    }));
    router.add_provider(Arc::new(CatalogProvider {
        name: "pb",
        models: &["shared"],
        seen: seen.clone(),
    }));
    state.llm_router.swap(router);
    (state, seen)
}

fn stored(state: &crate::AppState, session_id: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::get_session_model(&conn, session_id).unwrap()
}

fn agent_model(state: &crate::AppState, agent_id: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::get_agent(&conn, agent_id)
        .unwrap()
        .unwrap()
        .model
}

#[tokio::test]
async fn set_session_model_changes_only_the_bound_session() {
    let (state, _) = state();

    let snapshot = state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();

    assert_eq!(snapshot.configured_model.as_deref(), Some("pa:big"));
    assert_eq!(snapshot.current_model, "pa:big");
    assert_eq!(snapshot.default_model, "pa:small");
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));
    assert_eq!(stored(&state, "sess-b"), None);
    assert_eq!(agent_model(&state, "a1").as_deref(), Some("pa:small"));

    let listed = state
        .list_session_models("a1", "sess-a")
        .await
        .unwrap()
        .snapshot;
    assert_eq!(listed.configured_model.as_deref(), Some("pa:big"));
    assert_eq!(listed.current_model, "pa:big");
    assert_eq!(listed.default_model, "pa:small");
    let other = state
        .list_session_models("a1", "sess-b")
        .await
        .unwrap()
        .snapshot;
    assert_eq!(other.configured_model, None);
    assert_eq!(other.current_model, "pa:small");

    // Idempotent retry.
    let again = state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "pa:big")
        .await
        .unwrap();
    assert_eq!(again.current_model, "pa:big");
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));
}

#[tokio::test]
async fn invalid_session_models_are_rejected_without_writes() {
    let (state, _) = state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();

    assert_eq!(
        state
            .set_session_model("a1", "sess-a", TurnModelProfile::Default, "missing")
            .await,
        Err(ModelAdminError::NotFound)
    );
    assert_eq!(
        state
            .set_session_model("a1", "sess-a", TurnModelProfile::Default, "shared")
            .await,
        Err(ModelAdminError::Ambiguous)
    );
    assert!(matches!(
        state
            .set_session_model("a1", "sess-a", TurnModelProfile::Default, "nobudget")
            .await,
        Err(ModelAdminError::Validation(_))
    ));
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));
    assert_eq!(agent_model(&state, "a1").as_deref(), Some("pa:small"));
}

#[tokio::test]
async fn session_commands_require_the_agent_as_the_only_member() {
    let (state, _) = state();

    for session_id in ["sess-shared", "sess-other", "missing"] {
        assert_eq!(
            state
                .set_session_model("a1", session_id, TurnModelProfile::Default, "big")
                .await,
            Err(ModelAdminError::TargetUnavailable),
            "{session_id}"
        );
        assert_eq!(
            state
                .reset_session_model("a1", session_id, TurnModelProfile::Default)
                .await,
            Err(ModelAdminError::TargetUnavailable),
            "{session_id}"
        );
        assert_eq!(
            state
                .list_session_models("a1", session_id)
                .await
                .map(|_| ()),
            Err(ModelAdminError::TargetUnavailable),
            "{session_id}"
        );
    }
    assert_eq!(stored(&state, "sess-shared"), None);
    assert_eq!(stored(&state, "sess-other"), None);
    assert_eq!(agent_model(&state, "a1").as_deref(), Some("pa:small"));
}

#[tokio::test]
async fn reset_session_model_follows_agent_and_keeps_override_when_inherited_is_invalid() {
    let (state, _) = state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();

    // Inherited model without a budget: reset is refused and the override stays.
    {
        let conn = state.db.lock().unwrap();
        agent(&conn, "a1", Some("pa:nobudget"));
    }
    assert!(matches!(
        state
            .reset_session_model("a1", "sess-a", TurnModelProfile::Default)
            .await,
        Err(ModelAdminError::Validation(_))
    ));
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));

    {
        let conn = state.db.lock().unwrap();
        agent(&conn, "a1", Some("pa:small"));
    }
    let snapshot = state
        .reset_session_model("a1", "sess-a", TurnModelProfile::Default)
        .await
        .unwrap();
    assert_eq!(snapshot.configured_model, None);
    assert_eq!(snapshot.current_model, "pa:small");
    assert_eq!(snapshot.default_model, "pa:small");
    assert_eq!(stored(&state, "sess-a"), None);
    assert_eq!(agent_model(&state, "a1").as_deref(), Some("pa:small"));

    // Idempotent retry.
    let again = state
        .reset_session_model("a1", "sess-a", TurnModelProfile::Default)
        .await
        .unwrap();
    assert_eq!(again.current_model, "pa:small");
}

async fn session_turn(state: &crate::AppState, mid_turn_change: Option<&str>) {
    profile_turn(state, TurnModelProfile::Default, mid_turn_change).await;
}

async fn profile_turn(
    state: &crate::AppState,
    profile: TurnModelProfile,
    mid_turn_change: Option<&str>,
) {
    let db = state.db.clone();
    let mid_turn_change = mid_turn_change.map(str::to_string);
    let result = run_session_turn(
        state,
        "sess-a",
        "a1",
        profile,
        "sys",
        "",
        |raw| raw.to_string(),
        |conversation| {
            // The session model changes after the turn read it (a `/model set` landing mid-turn).
            if let Some(model) = &mid_turn_change {
                let conn = db.lock().unwrap();
                opencrab_db::queries::set_session_model(&conn, "sess-a", Some(model)).unwrap();
            }
            RunRequest::new(
                "a1",
                "a1",
                "sess-a",
                "sys",
                conversation,
                "test",
                CallerIdentity::Owner,
            )
        },
    )
    .await
    .expect("turn ran");
    result.expect("turn succeeded");
}

#[tokio::test]
async fn session_turn_pins_the_model_read_at_turn_start() {
    let (state, seen) = state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();

    // Mid-turn the session switches to a model without any budget row: if anything in this turn
    // (envelope, output cap, engine model) re-read the setting, the turn would fail or change.
    session_turn(&state, Some("pa:nobudget")).await;
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![("big".to_string(), Some(8_000))]
    );

    // The next turn of the session uses the persisted change.
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_model(&conn, "sess-a", Some("pa:small")).unwrap();
    }
    session_turn(&state, None).await;
    assert_eq!(
        seen.lock().unwrap().last().cloned(),
        Some(("small".to_string(), Some(1_000)))
    );
}

#[tokio::test]
async fn session_turn_model_lookup_failure_stops_the_turn() {
    let (state, seen) = state();

    assert!(state
        .session_turn_model("a1", "missing", TurnModelProfile::Default)
        .is_err());
    let result = run_session_turn(
        &state,
        "missing",
        "a1",
        TurnModelProfile::Default,
        "sys",
        "",
        |raw| raw.to_string(),
        |conversation| {
            RunRequest::new(
                "a1",
                "a1",
                "missing",
                "sys",
                conversation,
                "test",
                CallerIdentity::Owner,
            )
        },
    )
    .await;
    assert!(matches!(result, Some(Err(_))));
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unpinned_process_entry_resolves_the_session_model() {
    let (state, seen) = state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();

    crate::process::run_agent_response(
        &state,
        RunRequest::new(
            "a1",
            "a1",
            "sess-a",
            "sys",
            "hi",
            "test",
            CallerIdentity::Owner,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![("big".to_string(), Some(8_000))]
    );

    // A session lookup failure is an error, not the agent or server default.
    assert!(crate::process::run_agent_response(
        &state,
        RunRequest::new(
            "a1",
            "a1",
            "missing",
            "sys",
            "hi",
            "test",
            CallerIdentity::Owner
        ),
    )
    .await
    .is_err());
    assert_eq!(seen.lock().unwrap().len(), 1);
}

// ---- D-1081: realtime slot ----

/// `pa:light` is 200k / 2k and `pa:huge` 1.05M / 8k (both reach the 80k cap of the default policy).
fn realtime_state() -> (crate::AppState, Seen) {
    let (state, seen) = state();
    {
        let conn = state.db.lock().unwrap();
        pricing(&conn, "light", 200_000, 2_000);
        pricing(&conn, "huge", 1_050_000, 8_000);
    }
    (state, seen)
}

fn stored_realtime(state: &crate::AppState, session_id: &str) -> Option<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::get_session_realtime_model(&conn, session_id).unwrap()
}

#[tokio::test]
async fn realtime_turn_pins_the_realtime_slot_and_default_turn_the_session_model() {
    let (state, seen) = realtime_state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_realtime_model(&conn, "sess-a", Some("pa:light"))
            .unwrap();
    }

    // A mid-turn change of the realtime slot does not affect the running realtime turn.
    let db = state.db.clone();
    let change = move || {
        let conn = db.lock().unwrap();
        opencrab_db::queries::set_session_realtime_model(&conn, "sess-a", Some("pa:nobudget"))
            .unwrap();
    };
    let result = run_session_turn(
        &state,
        "sess-a",
        "a1",
        TurnModelProfile::Realtime,
        "sys",
        "",
        |raw| raw.to_string(),
        |conversation| {
            change();
            RunRequest::new(
                "a1",
                "a1",
                "sess-a",
                "sys",
                conversation,
                "test",
                CallerIdentity::Owner,
            )
        },
    )
    .await
    .expect("turn ran");
    result.expect("turn succeeded");
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![("light".to_string(), Some(2_000))]
    );

    session_turn(&state, None).await;
    assert_eq!(
        seen.lock().unwrap().last().cloned(),
        Some(("big".to_string(), Some(8_000)))
    );

    // Unset realtime slot: realtime turns follow the session model.
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_session_realtime_model(&conn, "sess-a", None).unwrap();
    }
    profile_turn(&state, TurnModelProfile::Realtime, None).await;
    assert_eq!(
        seen.lock().unwrap().last().cloned(),
        Some(("big".to_string(), Some(8_000)))
    );
}

#[tokio::test]
async fn realtime_set_list_reset_touch_only_the_realtime_slot() {
    let (state, _) = realtime_state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "big")
        .await
        .unwrap();

    let set = state
        .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "light")
        .await
        .unwrap();
    assert_eq!(set.configured_model.as_deref(), Some("pa:light"));
    assert_eq!(set.current_model, "pa:light");
    assert_eq!(set.default_model, "pa:big");
    assert_eq!(
        stored_realtime(&state, "sess-a").as_deref(),
        Some("pa:light")
    );
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));
    assert_eq!(stored_realtime(&state, "sess-b"), None);

    let list = state.list_session_models("a1", "sess-a").await.unwrap();
    assert_eq!(list.snapshot.configured_model.as_deref(), Some("pa:big"));
    assert_eq!(list.snapshot.current_model, "pa:big");
    assert_eq!(list.realtime_configured_model.as_deref(), Some("pa:light"));
    assert_eq!(list.realtime_current_model, "pa:light");
    let other = state.list_session_models("a1", "sess-b").await.unwrap();
    assert_eq!(other.realtime_configured_model, None);
    assert_eq!(other.realtime_current_model, "pa:small");

    let reset = state
        .reset_session_model("a1", "sess-a", TurnModelProfile::Realtime)
        .await
        .unwrap();
    assert_eq!(reset.configured_model, None);
    assert_eq!(reset.current_model, "pa:big");
    assert_eq!(reset.default_model, "pa:big");
    assert_eq!(stored_realtime(&state, "sess-a"), None);
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));

    // Idempotency compares with the realtime slot: setting it to the session model's value
    // still writes the realtime slot.
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "pa:big")
        .await
        .unwrap();
    assert_eq!(stored_realtime(&state, "sess-a").as_deref(), Some("pa:big"));
    let again = state
        .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "pa:big")
        .await
        .unwrap();
    assert_eq!(again.configured_model.as_deref(), Some("pa:big"));
    assert_eq!(again.default_model, "pa:big");
}

#[tokio::test]
async fn realtime_commands_require_the_agent_as_the_only_member() {
    let (state, _) = realtime_state();
    for session_id in ["sess-shared", "sess-other", "missing"] {
        assert_eq!(
            state
                .set_session_model("a1", session_id, TurnModelProfile::Realtime, "light")
                .await,
            Err(ModelAdminError::TargetUnavailable),
            "{session_id}"
        );
        assert_eq!(
            state
                .reset_session_model("a1", session_id, TurnModelProfile::Realtime)
                .await,
            Err(ModelAdminError::TargetUnavailable),
            "{session_id}"
        );
    }
    assert_eq!(stored_realtime(&state, "sess-shared"), None);
    assert_eq!(stored_realtime(&state, "sess-other"), None);
}

/// The realtime slot may not have a lower `input_high` than the session's default-profile model
/// under the live context-budget policy; equal is accepted.
#[tokio::test]
async fn realtime_model_with_lower_input_high_is_rejected_without_writes() {
    let (state, _) = realtime_state();
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "huge")
        .await
        .unwrap();

    // pa:small (50k) has input_high 25k < 80k.
    assert!(matches!(
        state
            .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "small")
            .await,
        Err(ModelAdminError::Validation(_))
    ));
    assert_eq!(stored_realtime(&state, "sess-a"), None);
    assert!(matches!(
        state
            .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "nobudget")
            .await,
        Err(ModelAdminError::Validation(_))
    ));
    assert_eq!(stored_realtime(&state, "sess-a"), None);

    // pa:light (200k) reaches the same 80k cap as the 1.05M session model.
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "light")
        .await
        .unwrap();
    assert_eq!(
        stored_realtime(&state, "sess-a").as_deref(),
        Some("pa:light")
    );
}

#[tokio::test]
async fn realtime_input_high_guard_uses_the_live_policy() {
    let (mut state, _) = realtime_state();
    let mut config = (*state.llm_config).clone();
    config.absolute_input_cap = 1_000_000;
    state.llm_config = Arc::new(config);
    state
        .set_session_model("a1", "sess-a", TurnModelProfile::Default, "huge")
        .await
        .unwrap();

    // With the raised cap: light 100k < huge 525k.
    assert!(matches!(
        state
            .set_session_model("a1", "sess-a", TurnModelProfile::Realtime, "light")
            .await,
        Err(ModelAdminError::Validation(_))
    ));
    assert_eq!(stored_realtime(&state, "sess-a"), None);
}
