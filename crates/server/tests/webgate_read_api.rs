//! §3.2 read 投影と §4.2 ページング。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn seeded() -> (axum::Router, String, String) {
    let conn = opencrab_db::init_memory().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "a1".into(),
            name: "a1".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let logical = "web-a1-c1".to_string();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: logical.clone(),
            mode: "web".into(),
            theme: "legacy-theme".into(),
            phase: "divergent".into(),
            turn_number: 1,
            status: "active".into(),
            participant_ids_json: r#"["a1"]"#.into(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: Some(r#"{"keep":true}"#.into()),
        },
    )
    .unwrap();
    let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let physical = format!("extgate-{binding}");
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES ('aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', 'web', ?1, 1, 1, 'e30=', '0000000000000000000000000000000000000000000000000000000000000000', 1, 1)",
        [subject],
    )
    .unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: physical.clone(),
            mode: "extgate".into(),
            theme: logical.clone(),
            phase: "convergent".into(),
            turn_number: 9,
            status: "active".into(),
            participant_ids_json: r#"["a1"]"#.into(),
            facilitator_id: None,
            done_count: 2,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES (?1, 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', ?2, 1)",
        rusqlite::params![binding, logical],
    )
    .unwrap();
    opencrab_db::queries::insert_session_log(
        &conn,
        &opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: "a1".into(),
            session_id: physical.clone(),
            log_type: "speech".into(),
            content: "moved".into(),
            speaker_id: Some("a1".into()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap();
    (app_from_conn(conn), logical, physical)
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null)),
    )
}

#[tokio::test]
async fn list_sessions_hides_physical_and_keeps_logical() {
    let (app, logical, physical) = seeded();
    let (status, body) = get(app, "/api/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&logical.as_str()));
    assert!(!ids.contains(&physical.as_str()));
    let row = body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == logical)
        .unwrap();
    assert_eq!(row["agent_ids"], serde_json::json!(["a1"]));
}

#[tokio::test]
async fn get_session_projects_physical_state_with_gateway_bound() {
    let (app, logical, _) = seeded();
    let (status, body) = get(app, &format!("/api/sessions/{logical}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], logical);
    assert_eq!(body["theme"], "legacy-theme");
    assert_eq!(body["turn_number"], 9);
    assert_eq!(body["phase"], "convergent");
    assert_eq!(body["gateway_bound"], true);
    assert_eq!(body["web_binding_state"], "unavailable");
    assert_eq!(body["binding_address"], logical);
    assert_eq!(body["agent_ids"], serde_json::json!(["a1"]));
}

#[tokio::test]
async fn get_session_by_physical_id_exposes_binding_address() {
    let (app, logical, physical) = seeded();
    let (status, body) = get(app, &format!("/api/sessions/{physical}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["binding_address"], logical);
    assert_eq!(body["gateway_bound"], true);
}

#[tokio::test]
async fn get_session_absent_is_404() {
    let (app, _, _) = seeded();
    let (status, _) = get(app, "/api/sessions/no-such").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn logs_read_physical_only() {
    let (app, logical, _) = seeded();
    let (status, body) = get(app, &format!("/api/sessions/{logical}/logs")).await;
    assert_eq!(status, StatusCode::OK);
    let logs = body.as_array().unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["content"], "moved");
}

fn app_from_conn(conn: rusqlite::Connection) -> axum::Router {
    let db = opencrab_db::Db::from_connection(conn);
    let state = opencrab_server::AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(opencrab_llm::router::LlmRouter::new()),
        llm_config: std::sync::Arc::new(toml::from_str("default_provider = \"\"").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: std::sync::Arc::new(Default::default()),
        voice_runtime: std::sync::Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:test".to_string(),
        tools_config: std::sync::Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        typed_history_enabled: false,
        typed_history_drop_directive: false,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
        memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
        memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
        memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        intake: std::sync::Arc::new(Default::default()),
        intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        gateways: std::sync::Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
        session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
        subtask_notifiers: std::sync::Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: std::sync::Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: Default::default(),
        scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        heartbeat_config_rx: opencrab_server::disconnected_heartbeat_config_rx(Default::default()),
        timed_fire_router: std::sync::Arc::new(opencrab_actions::TimedFireRouter::new()),
        progress_debounce: std::sync::Arc::new(
            opencrab_server::subtask_registries::ProgressDebounce::new(),
        ),
    };
    opencrab_server::create_router(state)
}

#[tokio::test]
async fn list_sessions_honors_limit_and_before() {
    let conn = opencrab_db::init_memory().unwrap();
    for i in 0..3 {
        opencrab_db::queries::insert_session(
            &conn,
            &opencrab_db::queries::SessionRow {
                id: format!("s{i}"),
                mode: "autonomous".into(),
                theme: format!("t{i}"),
                phase: "divergent".into(),
                turn_number: 0,
                status: "active".into(),
                participant_ids_json: "[]".into(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let app = app_from_conn(conn);
    let (status, first) = get(app.clone(), "/api/sessions?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = first.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let before = arr[0]["id"].as_str().unwrap();
    let (status, second) = get(app, &format!("/api/sessions?limit=1&before={before}")).await;
    assert_eq!(status, StatusCode::OK);
    let arr2 = second.as_array().unwrap();
    assert_eq!(arr2.len(), 1);
    assert_ne!(arr2[0]["id"], before);
}

#[tokio::test]
async fn list_and_detail_agent_ids_from_membership_sorted_updated_at() {
    let mut conn = opencrab_db::init_memory().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "a1".into(),
            name: "a1".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: "intake-old".into(),
            mode: "intake".into(),
            theme: "mail".into(),
            phase: "active".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: "[]".into(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_sessions (agent_id, session_id) VALUES ('a1', 'intake-old')",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET updated_at = '1999-01-01T00:00:00+00:00' WHERE id = 'intake-old'",
        [],
    )
    .unwrap();
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let instance = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let logical = "web-a1-fresh";
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'web', ?2, 1, 1, 'e30=', '0000000000000000000000000000000000000000000000000000000000000000', 1, 1)",
        rusqlite::params![instance, subject],
    )
    .unwrap();
    {
        let tx = conn.transaction().unwrap();
        opencrab_db::queries::create_gate_binding_in_tx(
            &tx,
            binding,
            instance,
            logical,
            logical,
            1_700_000_000_000_000_000,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let physical = format!("extgate-{binding}");
    let app = app_from_conn(conn);
    let (status, body) = get(app.clone(), "/api/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![logical, "intake-old"]);
    assert_eq!(body[0]["agent_ids"], serde_json::json!(["a1"]));
    assert_eq!(body[1]["agent_ids"], serde_json::json!(["a1"]));
    assert!(!ids.contains(&physical.as_str()));

    let (status, detail) = get(app.clone(), &format!("/api/sessions/{logical}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["agent_ids"], serde_json::json!(["a1"]));
    assert_eq!(detail["gateway_bound"], true);

    let (status, phys) = get(app, &format!("/api/sessions/{physical}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(phys["agent_ids"], serde_json::json!(["a1"]));
    assert_eq!(phys["gateway_bound"], true);
    assert_eq!(phys["binding_address"], logical);
}
