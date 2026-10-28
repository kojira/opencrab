// Conformance harness waits (split out of support.rs for the file-size limit).

async fn wait_client_bound(client: &InstanceClient, address: &str, binding_id: &str) {
    for _ in 0..80 {
        if client.binding_for_address(address).await.as_deref() == Some(binding_id) {
            return;
        }
        tokio::time::advance(Duration::from_millis(5)).await;
    }
    panic!("bind ack");
}
/// live entry が消える（切断が反映される）まで待つ。
async fn wait_not_live(h: &Harness, instance_id: &str) {
    for _ in 0..200 {
        if h.state.lock_registry().unwrap().get(instance_id).is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("live entry が消えない: {instance_id}");
}

fn insert_named_session(h: &Harness, id: &str) {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &SessionRow {
            id: id.into(),
            mode: "solo".into(),
            theme: id.into(),
            phase: "convergent".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: r#"["agent-1"]"#.into(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

/// The bind of `binding_id` is acknowledged in the live registry.
async fn wait_acknowledged(h: &Harness, instance_id: &str, binding_id: &str) {
    for _ in 0..50 {
        let acked = h
            .state
            .lock_registry()
            .unwrap()
            .get(instance_id)
            .is_some_and(|e| e.acknowledged.contains(binding_id));
        if acked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("bind not acknowledged: {binding_id}");
}

/// D-1085 §12: a run's declared gateway operation names and whether it has a dispatch sink.
type ToolSources = (Vec<String>, bool);

fn tool_sources(req: &RunRequest) -> ToolSources {
    let op_names = req
        .gateway_actions
        .as_ref()
        .map(|ga| ga.definitions().into_iter().map(|d| d.name).collect())
        .unwrap_or_default();
    (op_names, req.completion_sink.is_some())
}

/// D-1096: a running subtask entry of `parent_session_id` (its task is `abort_handle`).
fn test_spawned_subtask(
    abort_handle: tokio::task::AbortHandle,
    parent_session_id: &str,
) -> opencrab_actions::SpawnedSubtask {
    opencrab_actions::SpawnedSubtask {
        abort_handle,
        session_id: "subtask-sub-1".to_string(),
        parent_session_id: parent_session_id.to_string(),
        agent_id: "agent-1".to_string(),
        label: "job".to_string(),
        tool_name: "spawn_subtask".to_string(),
        started_at: std::time::Instant::now(),
        reply_target: None,
        caller: CallerIdentity::Owner,
        lifecycle: opencrab_actions::SubtaskLifecycle::new(),
        steerable: false,
    }
}
