//! D-1135: the agent's `select_llm` is the same change as the owner's `/model`: it writes the
//! conversation's model (`sessions.model`) and the conversation's next turns run on it.

use super::*;

fn turn_context(
    state: &crate::AppState,
    session_id: &str,
    turn_model: &str,
    workspace_root: &std::path::Path,
) -> opencrab_actions::ActionContext {
    let workspace = opencrab_core::workspace::Workspace::from_root(workspace_root).unwrap();
    opencrab_actions::ActionContext {
        caller: CallerIdentity::Owner,
        agent_id: "a1".to_string(),
        agent_name: "a1".to_string(),
        session_id: Some(session_id.to_string()),
        db: state.db.clone(),
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: state.default_model(),
            active_model: Some(turn_model.to_string()),
            available_providers: vec!["pa".to_string(), "pb".to_string()],
            gateway: "test".to_string(),
        })),
    }
}

#[tokio::test]
async fn select_llm_changes_the_session_model_and_the_next_turn_uses_it() {
    use opencrab_actions::Action as _;
    let (state, seen) = state();
    let dir = tempfile::tempdir().unwrap();
    let models: Arc<dyn ModelAdministration> = Arc::new(state.clone());
    let ctx = turn_context(&state, "sess-a", "pa:small", dir.path());

    let result = opencrab_actions::llm_selection::SelectLlmAction::new(Some(models.clone()))
        .execute(
            &serde_json::json!({"model_alias": "big", "reason": "r"}),
            &ctx,
        )
        .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        result.data.as_ref().unwrap()["conversation_model"],
        "pa:big"
    );
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));
    assert_eq!(stored(&state, "sess-b"), None);
    assert_eq!(agent_model(&state, "a1").as_deref(), Some("pa:small"));

    let info = opencrab_actions::common::GetSystemInfoAction::new(Some(models.clone()))
        .execute(&serde_json::json!({}), &ctx)
        .await
        .data
        .unwrap();
    assert_eq!(info["current_turn_model"], "pa:small");
    assert_eq!(info["conversation_model"], "pa:big");

    // The conversation's following turns run on the switched model.
    session_turn(&state, None).await;
    session_turn(&state, None).await;
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![
            ("big".to_string(), Some(8_000)),
            ("big".to_string(), Some(8_000))
        ]
    );

    // An unresolvable name changes nothing.
    let rejected = opencrab_actions::llm_selection::SelectLlmAction::new(Some(models))
        .execute(
            &serde_json::json!({"model_alias": "missing", "reason": "r"}),
            &ctx,
        )
        .await;
    assert!(!rejected.success);
    assert!(rejected.error.unwrap().contains("pa:small"));
    assert_eq!(stored(&state, "sess-a").as_deref(), Some("pa:big"));
}

#[tokio::test]
async fn select_llm_in_a_shared_session_changes_nothing() {
    use opencrab_actions::Action as _;
    let (state, _) = state();
    let dir = tempfile::tempdir().unwrap();
    let ctx = turn_context(&state, "sess-shared", "pa:small", dir.path());

    let result =
        opencrab_actions::llm_selection::SelectLlmAction::new(Some(Arc::new(state.clone())))
            .execute(
                &serde_json::json!({"model_alias": "big", "reason": "r"}),
                &ctx,
            )
            .await;
    assert!(!result.success);
    assert_eq!(stored(&state, "sess-shared"), None);
}
