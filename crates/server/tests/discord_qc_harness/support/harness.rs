use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn register_mock_pricing(db: &opencrab_db::Db) {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_model_pricing(
        &conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: "mock".to_string(),
            model: "gpt-4o".to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(200_000),
            max_output_tokens: Some(4_096),
        },
    )
    .expect("test model_pricing");
}

fn upsert_test_agent(db: &opencrab_db::Db) -> i64 {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: AGENT_ID.into(),
            name: "QC".into(),
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
    conn.query_row(
        "SELECT subject_id FROM agents WHERE agent_id = ?1",
        [AGENT_ID],
        |r| r.get(0),
    )
    .unwrap()
}

fn build_app_state(db: opencrab_db::Db, provider: Arc<dyn LlmProvider>) -> AppState {
    let mut router = LlmRouter::new();
    router.add_provider(provider);
    router.set_default_provider("mock");
    AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir()
            .join("opencrab_discord_qc")
            .to_string_lossy()
            .to_string(),
        #[cfg(any())]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
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
    }
}

pub(crate) struct Core {
    pub(crate) extgate: Arc<ExtgateState>,
    pub(crate) sock: PathBuf,
    pub(crate) subject_id: i64,
    /// #915: execute_shell を実走させる tools_config 等を per-test で触れるよう AppState を保持。
    pub(crate) state: AppState,
    _dir: tempfile::TempDir,
}

/// #915: echo と sleep だけを許可した shell 有効 tools 設定（date 相当＝echo・sleep 相当＝sleep）。
pub(crate) fn shell_enabled_tools_config() -> opencrab_actions::tools::ToolsConfig {
    opencrab_actions::tools::ToolsConfig {
        enabled: true,
        shell: Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: vec!["echo".to_string(), "sleep".to_string()],
            ..Default::default()
        }),
    }
}

/// 実 serve_uds core + 実 AppState を UDS で立ち上げる。nostr hooks は張らない（discord は generic 経路）。
pub(crate) async fn start_core(provider: Arc<dyn LlmProvider>) -> Core {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    register_mock_pricing(&db);
    let subject_id = upsert_test_agent(&db);
    let extgate = Arc::new(ExtgateState::new_protected(db.clone()));
    let state = build_app_state(db.clone(), provider);
    // #925: 本番と同じ descriptor 登録（`register_production_descriptors`）＋ V3 heartbeat 受け口
    // （`ExtgateTimedFireSink`）を実型で配線する。これで scheduler seam（resolve_target →
    // run_one_heartbeat）が extgate session を解決し発火できる（未登録なら resolve_target None で
    // 配送 0＝赤）。descriptor は本番経路で登録するので、`register_production_descriptors` から
    // ExtgateFire が抜けると本ハーネスの heartbeat も赤になる（配線漏れを捕捉）。
    state
        .timed_fire_router
        .register_sink(Arc::new(opencrab_extgate::ExtgateTimedFireSink::new(
            extgate.clone(),
            state.clone(),
        )));

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    {
        let listen_state = Arc::clone(&extgate);
        let runtime = state.clone();
        let path = sock.clone();
        tokio::spawn(async move {
            let _ = serve_uds(listen_state, runtime, path).await;
        });
    }
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    Core {
        extgate,
        sock,
        subject_id,
        state,
        _dir: dir,
    }
}

async fn provision(
    core: &Core,
    instance_id: &str,
    config_b64: &str,
    bindings: Vec<(String, String)>,
) {
    let grant = {
        let mut conn = core.extgate.db.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM gate_instances WHERE instance_id=?1",
                [instance_id],
                |row| row.get(0),
            )
            .unwrap();
        (exists == 0).then(|| {
            opencrab_db::queries::issue_subject_association_grant(
                &mut conn,
                AGENT_ID,
                core.subject_id,
                i64::MAX,
                100,
            )
            .unwrap()
        })
    };
    let mut stream = UnixStream::connect(&core.sock).await.unwrap();
    let mut request = serde_json::json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "m": "provision",
        "instance_id": instance_id,
        "kind_id": "discord",
        "subject_id": core.subject_id,
        "adopt_existing": false,
        "enabled": true,
        "config_b64": config_b64,
        "bindings": bindings.into_iter().map(|(binding_id, address)| serde_json::json!({"binding_id": binding_id, "address": address})).collect::<Vec<_>>(),
    });
    if let Some(grant) = grant {
        request["subject_grant"] = serde_json::json!(grant);
    }
    let mut bytes = serde_json::to_vec(&request).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    let mut response = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).await.unwrap();
        if byte[0] == b'\n' {
            break;
        }
        response.push(byte[0]);
    }
    let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(response["m"], "provisioned", "{response}");
}

pub(crate) async fn put_instance(core: &Core, instance_id: &str, config_b64: &str) {
    provision(core, instance_id, config_b64, Vec::new()).await;
}

pub(crate) async fn put_binding(core: &Core, binding_id: &str, instance_id: &str, address: &str) {
    let (config_b64, mut bindings) = {
        let conn = core.extgate.db.lock().unwrap();
        let config_b64: String = conn
            .query_row(
                "SELECT config_b64 FROM gate_instances WHERE instance_id=?1",
                [instance_id],
                |row| row.get(0),
            )
            .unwrap();
        let mut statement = conn
            .prepare("SELECT binding_id, address FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL ORDER BY binding_id")
            .unwrap();
        let bindings = statement
            .query_map([instance_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        (config_b64, bindings)
    };
    if !bindings.iter().any(|(id, _)| id == binding_id) {
        bindings.push((binding_id.to_string(), address.to_string()));
    }
    provision(core, instance_id, &config_b64, bindings).await;
}

pub(crate) fn discord_config() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "agent_id": AGENT_ID,
        "self_bot_id": SELF_BOT,
        "name": "crab",
        "delivery_mode": "say",
        "access": {"owners": [AUTHOR]},
    }))
    .unwrap()
}

/// instance + binding を登録し、discord gateway（fake_events + dry_run）を起動して bind ack を待つ。
pub(crate) async fn wire_instance(core: &Core, fixture: &Fixture) -> Arc<InstanceClient> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = address();

    put_instance(core, &instance_id, &config_b64).await;
    put_binding(core, &binding_id, &instance_id, &addr).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        addresses: vec![addr.clone()],
        config_b64,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(
        core.sock.clone(),
        &place,
        &config_bytes,
        None,
        overrides,
        None,
    )
    .expect("spawn_instance");

    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&addr).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない");
    client
}

/// 指定チャンネルで instance+binding を張り gateway を起動する（#915: typing 隔離テスト用）。
/// BUFFER は binary 内で共有・累積かつ CI は並列実行なので、typing（scope key を持たない capture）
/// を他テストと分離するために、他テストが使わない専用チャンネルへ束ねる。
pub(crate) async fn wire_instance_on_channel(
    core: &Core,
    fixture: &Fixture,
    channel: &str,
) -> Arc<InstanceClient> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let binding_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = format!("discord-{AGENT_ID}-{GUILD}-{channel}");

    put_instance(core, &instance_id, &config_b64).await;
    put_binding(core, &binding_id, &instance_id, &addr).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        addresses: vec![addr.clone()],
        config_b64,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(
        core.sock.clone(),
        &place,
        &config_bytes,
        None,
        overrides,
        None,
    )
    .expect("spawn_instance");

    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&addr).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない（専用チャンネル {channel}）");
    client
}

pub(crate) fn count_kind(buf: &Arc<Mutex<Vec<Captured>>>, kind: &str) -> usize {
    captured(buf).iter().filter(|c| c.kind == kind).count()
}

/// 指定チャンネルに限定した kind 別キャプチャ数（#915: typing を専用チャンネルで数える）。
pub(crate) fn count_kind_on_channel(
    buf: &Arc<Mutex<Vec<Captured>>>,
    kind: &str,
    channel: &str,
) -> usize {
    captured(buf)
        .iter()
        .filter(|c| c.kind == kind && c.channel == channel)
        .count()
}

pub(crate) fn own_speech_rows(core: &Core) -> i64 {
    let conn = core.extgate.db.lock().unwrap();
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' AND speaker_id='{AGENT_ID}'"
        ),
        [],
        |r| r.get(0),
    )
    .unwrap()
}
